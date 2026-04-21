//! Value type for the Bop interpreter.
//!
//! Heap-allocating variants use newtypes with private fields.
//! The only way to construct them is through the tracked constructors
//! (`Value::new_str`, `Value::new_array`, `Value::new_dict`), which
//! call `bop_alloc`. This is enforced by the type system — code outside
//! this module cannot access the private inner fields.

#[cfg(feature = "no_std")]
use alloc::{boxed::Box, format, rc::Rc, string::{String, ToString}, vec::Vec};

#[cfg(not(feature = "no_std"))]
use std::rc::Rc;

use core::cell::RefCell;

use crate::memory::{bop_alloc, bop_dealloc};
use crate::parser::Stmt;

// ─── Tracked newtypes ──────────────────────────────────────────────────────
//
// Private inner fields prevent direct construction from outside this module.

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BopStr(String);

#[derive(Debug)]
pub struct BopArray(Vec<Value>);

#[derive(Debug)]
pub struct BopDict(Vec<(String, Value)>);

/// A user-defined struct value. Carries the module it was
/// declared in plus the bare type name, so two modules that
/// happen to declare `struct Color { ... }` independently
/// produce distinct values even when they share a name. The
/// module path is `<root>` for the top-level program and
/// `<builtin>` for engine-registered builtins like
/// `RuntimeError`; for user modules it's the dot-joined `use`
/// path (`"std.math"`, `"game.entity"`, …). Fields are stored
/// in declaration order so iteration and `Display` stay stable.
#[derive(Debug)]
pub struct BopStruct {
    module_path: String,
    type_name: String,
    fields: Vec<(String, Value)>,
}

/// A user-defined enum variant value — the concrete data side of
/// Bop's sum types. Like [`BopStruct`], it's identified by the
/// `(module_path, type_name)` pair, plus the selected variant's
/// name and payload. Two enums declared in different modules with
/// the same type name and even the same variants still compare
/// as distinct types.
#[derive(Debug)]
pub struct BopEnumVariant {
    module_path: String,
    type_name: String,
    variant: String,
    payload: EnumPayload,
}

/// Module path used for engine-registered builtins (`Result`,
/// `RuntimeError`). Surfaces wherever a struct / enum value
/// needs to carry its declaring module; the engines all agree
/// on this literal so patterns + equality line up across
/// walker / VM / AOT.
pub const BUILTIN_MODULE_PATH: &str = "<builtin>";

/// Module path used for types declared directly in the program
/// root (not in any imported module). Same literal across every
/// engine.
pub const ROOT_MODULE_PATH: &str = "<root>";

/// Runtime payload attached to a `BopEnumVariant`. Mirrors the
/// three variant shapes the parser recognises
/// (`VariantKind::{Unit, Tuple, Struct}`).
#[derive(Debug)]
pub enum EnumPayload {
    Unit,
    Tuple(Vec<Value>),
    Struct(Vec<(String, Value)>),
}

/// A Bop function value — the runtime representation of a closure
/// or a reified `fn foo(...) { ... }` declaration. Shared by `Rc`
/// so first-class usage (`let g = f; pass(f)`) is cheap.
///
/// The body is engine-opaque: the tree-walker produces an
/// [`FnBody::Ast`] for direct interpretation; the bytecode VM
/// produces an [`FnBody::Compiled`] carrying a pre-compiled body.
/// Each engine only ever dispatches its own variant.
pub struct BopFn {
    pub params: Vec<String>,
    /// Values captured from the enclosing scope at construction
    /// time, cloned by value. Free variables in the body that
    /// aren't parameters and aren't in this list fall through to
    /// the outer module / global lookup at call time.
    pub captures: Vec<(String, Value)>,
    pub body: FnBody,
    /// `Some(name)` when this `BopFn` is bound to its own name
    /// for self-reference (the lowering of `fn foo(...) { ... }`).
    /// Lambdas created from an `fn(...) { ... }` expression leave
    /// this `None`.
    pub self_name: Option<String>,
}

/// Engine-specific representation of a function body.
///
/// - The tree-walker creates `Ast` bodies and re-walks the AST on
///   every call.
/// - The bytecode VM creates `Compiled` bodies carrying a
///   pre-compiled form (typically `Rc<bop_vm::Chunk>`). `Rc<dyn
///   Any>` keeps `bop-lang` from taking a dep on any particular
///   engine crate.
///
/// An engine that only understands one variant errors cleanly
/// when handed the other, rather than silently misbehaving.
pub enum FnBody {
    Ast(Vec<Stmt>),
    Compiled(Rc<dyn core::any::Any + 'static>),
}

impl core::fmt::Debug for BopFn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BopFn")
            .field("params", &self.params)
            .field("captures", &self.captures.len())
            .field("body", &self.body)
            .field("self_name", &self.self_name)
            .finish()
    }
}

impl core::fmt::Debug for FnBody {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FnBody::Ast(stmts) => write!(f, "Ast({} stmts)", stmts.len()),
            FnBody::Compiled(_) => write!(f, "Compiled(<opaque>)"),
        }
    }
}

// ─── Value enum ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Value {
    /// 64-bit signed integer. The go-to type for counts,
    /// indices, and any arithmetic that wants exactness. Added
    /// in phase 6; produced by integer literals (`42`), the
    /// `int()` builtin, `len`, `range` elements, and the new
    /// `//` integer-division operator.
    Int(i64),
    /// 64-bit IEEE-754 float. Produced by decimal literals
    /// (`3.14`, `4.0`), the `float()` builtin, and by `/` on
    /// any numeric pair (Python-style: `/` always floats).
    Number(f64),
    Str(BopStr),
    Bool(bool),
    None,
    Array(BopArray),
    Dict(BopDict),
    Fn(Rc<BopFn>),
    // `Struct` and `EnumVariant` live behind a `Box` so the
    // `Value` enum stays compact (roughly the size of a `Vec`
    // header) rather than ballooning to the size of the widest
    // user-type variant. Keeps per-call stack frames small
    // enough for deep recursion to halt on the call-depth
    // counter before overflowing the native stack.
    Struct(Box<BopStruct>),
    EnumVariant(Box<BopEnumVariant>),
    /// Namespace value produced by an aliased `use` statement
    /// (`use std.math as m` binds `m` as a `Module`). Field
    /// access dispatches to the module's exported `let` / `fn`
    /// bindings; the runtime also consults the type list for
    /// `m.Type { ... }` / `m.Type::Variant(...)` forms so those
    /// namespaced constructors find the right declared type.
    Module(Rc<BopModule>),
    /// Lazy iterator. Cloning shares state (like `Value::Fn`) —
    /// `let b = a; a.next(); b.next()` advances the same
    /// underlying position, matching iterator semantics in
    /// Python / Rust / JS. See [`BopIter`] for the built-in
    /// variants; user-defined iterators are ordinary struct
    /// values that happen to implement `.next()`.
    Iter(Rc<RefCell<BopIter>>),
}

/// Built-in lazy iterator shapes. Each one holds a snapshot of
/// the source sequence plus a cursor; advancing via [`Self::next`]
/// yields items until exhausted. A user-defined iterator doesn't
/// need to live here — it's just a struct with a `.next()`
/// method, dispatched through the ordinary method path.
#[derive(Debug)]
pub enum BopIter {
    /// Over a cloned-off array snapshot. Subsequent mutation of
    /// the original array doesn't affect the iterator — matches
    /// how most scripting languages present iteration.
    Array { items: Vec<Value>, pos: usize },
    /// Over a string's Unicode code points, one item per code
    /// point. Each yielded value is a single-char string.
    String { chars: Vec<char>, pos: usize },
    /// Over a dict's keys, in declaration order. Same shape
    /// `for k in dict` uses when the receiver is a plain dict.
    Dict { keys: Vec<String>, pos: usize },
}

impl BopIter {
    /// Advance by one and return the next item, or `None` when
    /// the iterator is exhausted. Caller wraps the result in
    /// `Iter::Next(v)` / `Iter::Done` for user code.
    pub fn next(&mut self) -> Option<Value> {
        match self {
            BopIter::Array { items, pos } => {
                if *pos < items.len() {
                    let v = items[*pos].clone();
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            BopIter::String { chars, pos } => {
                if *pos < chars.len() {
                    let v = Value::new_str(chars[*pos].to_string());
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            BopIter::Dict { keys, pos } => {
                if *pos < keys.len() {
                    let v = Value::new_str(keys[*pos].clone());
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
        }
    }
}

impl Drop for BopIter {
    fn drop(&mut self) {
        // Release the buffer tracked at construction time. The
        // inner Values (for Array) free themselves through their
        // own Drop; strings inside Dict keys do the same via
        // `key_bytes` accounting below.
        match self {
            BopIter::Array { items, .. } => {
                bop_dealloc(items.capacity() * core::mem::size_of::<Value>());
            }
            BopIter::String { chars, .. } => {
                bop_dealloc(chars.capacity() * core::mem::size_of::<char>());
            }
            BopIter::Dict { keys, .. } => {
                let key_bytes: usize = keys.iter().map(|k| k.capacity()).sum();
                bop_dealloc(
                    keys.capacity() * core::mem::size_of::<String>() + key_bytes,
                );
            }
        }
    }
}

/// Exported surface of a module, as presented through an aliased
/// `use` statement. `Rc<BopModule>` is what a `Value::Module`
/// carries so cloning the Value stays cheap.
#[derive(Debug)]
pub struct BopModule {
    /// The dotted path the module was loaded from ("std.math",
    /// "game.entity", …). Useful for error messages.
    pub path: String,
    /// Exported `let` / `fn` / `const` bindings, in declaration
    /// order. Accessed via `m.name` field reads.
    pub bindings: Vec<(String, Value)>,
    /// Names of struct / enum types the module declared.
    /// Construction through the namespace (`m.Entity { ... }`)
    /// verifies the type name appears in this list before
    /// falling through to the engine's type registry.
    pub types: Vec<String>,
}

// ─── Tracked constructors ──────────────────────────────────────────────────
//
// These call bop_alloc() to track the allocation but do NOT check the limit.
// Enforcement happens at tick() via bop_memory_exceeded(). This means a single
// operation can overshoot the limit before the next tick catches it. High-risk
// operations (string repeat, string/array concat) use bop_would_exceed() as a
// preflight check in the evaluator to avoid this.

impl Value {
    pub fn new_str(s: String) -> Self {
        bop_alloc(s.capacity());
        Value::Str(BopStr(s))
    }

    pub fn new_array(items: Vec<Value>) -> Self {
        bop_alloc(items.capacity() * core::mem::size_of::<Value>());
        Value::Array(BopArray(items))
    }

    pub fn new_dict(entries: Vec<(String, Value)>) -> Self {
        let key_bytes: usize = entries.iter().map(|(k, _)| k.capacity()).sum();
        bop_alloc(entries.capacity() * core::mem::size_of::<(String, Value)>() + key_bytes);
        Value::Dict(BopDict(entries))
    }

    /// Build a user-defined struct value. `module_path` is the
    /// module in which the type was declared (`<root>` at the
    /// top level, `<builtin>` for engine-registered shapes like
    /// `RuntimeError`, or the dot-joined `use` path for user
    /// modules). Two structs are only the same type when both
    /// the module path *and* the type name match — so a
    /// `struct Color { ... }` declared in two separate modules
    /// produces genuinely distinct values.
    pub fn new_struct(
        module_path: String,
        type_name: String,
        fields: Vec<(String, Value)>,
    ) -> Self {
        let key_bytes: usize = fields.iter().map(|(k, _)| k.capacity()).sum();
        bop_alloc(
            module_path.capacity()
                + type_name.capacity()
                + fields.capacity() * core::mem::size_of::<(String, Value)>()
                + key_bytes,
        );
        Value::Struct(Box::new(BopStruct {
            module_path,
            type_name,
            fields,
        }))
    }

    /// Build a built-in iterator that yields each item of
    /// `items` in order. Cloning the returned `Value::Iter`
    /// shares the iteration cursor, so `let b = a; a.next()`
    /// advances `b` too.
    pub fn new_array_iter(items: Vec<Value>) -> Self {
        bop_alloc(items.capacity() * core::mem::size_of::<Value>());
        Value::Iter(Rc::new(RefCell::new(BopIter::Array { items, pos: 0 })))
    }

    /// Build a built-in iterator over a string's Unicode code
    /// points.
    pub fn new_string_iter(chars: Vec<char>) -> Self {
        bop_alloc(chars.capacity() * core::mem::size_of::<char>());
        Value::Iter(Rc::new(RefCell::new(BopIter::String { chars, pos: 0 })))
    }

    /// Build a built-in iterator over a dict's keys (declaration
    /// order).
    pub fn new_dict_iter(keys: Vec<String>) -> Self {
        let key_bytes: usize = keys.iter().map(|k| k.capacity()).sum();
        bop_alloc(keys.capacity() * core::mem::size_of::<String>() + key_bytes);
        Value::Iter(Rc::new(RefCell::new(BopIter::Dict { keys, pos: 0 })))
    }

    pub fn new_enum_unit(module_path: String, type_name: String, variant: String) -> Self {
        bop_alloc(module_path.capacity() + type_name.capacity() + variant.capacity());
        Value::EnumVariant(Box::new(BopEnumVariant {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Unit,
        }))
    }

    pub fn new_enum_tuple(
        module_path: String,
        type_name: String,
        variant: String,
        items: Vec<Value>,
    ) -> Self {
        bop_alloc(
            module_path.capacity()
                + type_name.capacity()
                + variant.capacity()
                + items.capacity() * core::mem::size_of::<Value>(),
        );
        Value::EnumVariant(Box::new(BopEnumVariant {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Tuple(items),
        }))
    }

    pub fn new_enum_struct(
        module_path: String,
        type_name: String,
        variant: String,
        fields: Vec<(String, Value)>,
    ) -> Self {
        let key_bytes: usize = fields.iter().map(|(k, _)| k.capacity()).sum();
        bop_alloc(
            module_path.capacity()
                + type_name.capacity()
                + variant.capacity()
                + fields.capacity() * core::mem::size_of::<(String, Value)>()
                + key_bytes,
        );
        Value::EnumVariant(Box::new(BopEnumVariant {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Struct(fields),
        }))
    }

    /// Build a tree-walker-ready closure value. The AST body moves
    /// into a shared [`BopFn`] behind an `Rc`; subsequent clones
    /// of the resulting `Value::Fn` just bump the refcount.
    pub fn new_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
    ) -> Self {
        Value::Fn(Rc::new(BopFn {
            params,
            captures,
            body: FnBody::Ast(body),
            self_name,
        }))
    }

    /// Build a closure value with an engine-opaque compiled body.
    /// Used by the bytecode VM (and any future engine) to carry
    /// its pre-compiled form inside a `Value::Fn` without
    /// `bop-lang` depending on the engine crate.
    pub fn new_compiled_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
    ) -> Self {
        Value::Fn(Rc::new(BopFn {
            params,
            captures,
            body: FnBody::Compiled(body),
            self_name,
        }))
    }
}

// ─── Clone (tracks allocations) ────────────────────────────────────────────
//
// For Array and Dict, the inner .clone() recursively clones each element,
// and each element's Clone impl calls bop_alloc for itself. We then ALSO
// bop_alloc for the Vec buffer. This is correct — the buffer and the elements
// are separate allocations that both need tracking.

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::Int(n) => Value::Int(*n),
            Value::Number(n) => Value::Number(*n),
            Value::Bool(b) => Value::Bool(*b),
            Value::None => Value::None,
            Value::Str(s) => {
                let cloned = s.0.clone();
                bop_alloc(cloned.capacity());
                Value::Str(BopStr(cloned))
            }
            Value::Array(arr) => {
                let cloned = arr.0.clone(); // each element's Clone tracks itself
                bop_alloc(cloned.capacity() * core::mem::size_of::<Value>());
                Value::Array(BopArray(cloned))
            }
            Value::Dict(d) => {
                let cloned = d.0.clone(); // each Value's Clone tracks itself
                let key_bytes: usize = cloned.iter().map(|(k, _)| k.capacity()).sum();
                bop_alloc(cloned.capacity() * core::mem::size_of::<(String, Value)>() + key_bytes);
                Value::Dict(BopDict(cloned))
            }
            Value::Struct(s) => {
                let cloned_mp = s.module_path.clone();
                let cloned_tn = s.type_name.clone();
                let cloned_fields = s.fields.clone();
                let key_bytes: usize =
                    cloned_fields.iter().map(|(k, _)| k.capacity()).sum();
                bop_alloc(
                    cloned_mp.capacity()
                        + cloned_tn.capacity()
                        + cloned_fields.capacity()
                            * core::mem::size_of::<(String, Value)>()
                        + key_bytes,
                );
                Value::Struct(Box::new(BopStruct {
                    module_path: cloned_mp,
                    type_name: cloned_tn,
                    fields: cloned_fields,
                }))
            }
            // Closures are reference-counted: cloning a Value::Fn
            // is O(1) and doesn't duplicate the body or captures.
            // Tracking the captures' memory happens once, at the
            // moment the BopFn is constructed (by `new_fn`), via
            // their own Value Clone/Drop hooks.
            Value::Fn(f) => Value::Fn(Rc::clone(f)),
            // Modules are reference-counted — same cheap clone as
            // fns. The `bindings` and `types` vectors track their
            // own memory when the `BopModule` is first built.
            Value::Module(m) => Value::Module(Rc::clone(m)),
            // Iterators are reference-counted and intentionally
            // share their cursor — cloning `a = b` doesn't fork
            // the iteration state, matching iterator semantics
            // in Python / Rust / JS. The buffer was tracked once
            // by the constructor and is dealloc'd by BopIter's
            // Drop when the last clone goes away.
            Value::Iter(it) => Value::Iter(Rc::clone(it)),
            Value::EnumVariant(e) => {
                let mp = e.module_path.clone();
                let tn = e.type_name.clone();
                let vn = e.variant.clone();
                let base = mp.capacity() + tn.capacity() + vn.capacity();
                let payload = match &e.payload {
                    EnumPayload::Unit => {
                        bop_alloc(base);
                        EnumPayload::Unit
                    }
                    EnumPayload::Tuple(items) => {
                        let cloned = items.clone();
                        bop_alloc(
                            base + cloned.capacity() * core::mem::size_of::<Value>(),
                        );
                        EnumPayload::Tuple(cloned)
                    }
                    EnumPayload::Struct(fields) => {
                        let cloned = fields.clone();
                        let key_bytes: usize =
                            cloned.iter().map(|(k, _)| k.capacity()).sum();
                        bop_alloc(
                            base
                                + cloned.capacity()
                                    * core::mem::size_of::<(String, Value)>()
                                + key_bytes,
                        );
                        EnumPayload::Struct(cloned)
                    }
                };
                Value::EnumVariant(Box::new(BopEnumVariant {
                    module_path: mp,
                    type_name: tn,
                    variant: vn,
                    payload,
                }))
            }
        }
    }
}

// ─── Drop (tracks deallocations) ───────────────────────────────────────────

impl Drop for Value {
    fn drop(&mut self) {
        match self {
            Value::Str(s) => bop_dealloc(s.0.capacity()),
            Value::Array(arr) => {
                bop_dealloc(arr.0.capacity() * core::mem::size_of::<Value>());
            }
            Value::Dict(d) => {
                let key_bytes: usize = d.0.iter().map(|(k, _)| k.capacity()).sum();
                bop_dealloc(d.0.capacity() * core::mem::size_of::<(String, Value)>() + key_bytes);
            }
            Value::Struct(s) => {
                let key_bytes: usize = s.fields.iter().map(|(k, _)| k.capacity()).sum();
                bop_dealloc(
                    s.module_path.capacity()
                        + s.type_name.capacity()
                        + s.fields.capacity()
                            * core::mem::size_of::<(String, Value)>()
                        + key_bytes,
                );
            }
            Value::EnumVariant(e) => {
                let base = e.module_path.capacity()
                    + e.type_name.capacity()
                    + e.variant.capacity();
                match &e.payload {
                    EnumPayload::Unit => bop_dealloc(base),
                    EnumPayload::Tuple(items) => bop_dealloc(
                        base + items.capacity() * core::mem::size_of::<Value>(),
                    ),
                    EnumPayload::Struct(fields) => {
                        let key_bytes: usize =
                            fields.iter().map(|(k, _)| k.capacity()).sum();
                        bop_dealloc(
                            base
                                + fields.capacity()
                                    * core::mem::size_of::<(String, Value)>()
                                + key_bytes,
                        );
                    }
                }
            }
            // Value::Fn drops by releasing its Rc. The inner
            // captures' Drop impls fire only when the refcount
            // reaches zero; no per-Value accounting here.
            _ => {}
        }
    }
}

// ─── Display ───────────────────────────────────────────────────────────────

impl core::fmt::Display for Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{}", n),
            Value::Number(n) => {
                if *n == (*n as i64 as f64) && *n - *n == 0.0 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{}", n)
                }
            }
            Value::Str(s) => write!(f, "{}", s.0),
            Value::Bool(b) => write!(f, "{}", b),
            Value::None => write!(f, "none"),
            Value::Array(items) => {
                write!(f, "[")?;
                for (i, item) in items.0.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item.inspect())?;
                }
                write!(f, "]")
            }
            Value::Dict(entries) => {
                write!(f, "{{")?;
                for (i, (k, v)) in entries.0.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{}\": {}", k, v.inspect())?;
                }
                write!(f, "}}")
            }
            Value::Fn(func) => match &func.self_name {
                Some(name) => write!(f, "<fn {}>", name),
                None => write!(f, "<fn>"),
            },
            Value::Module(m) => write!(f, "<module {}>", m.path),
            Value::Iter(it) => {
                // Peek at the inner state for a useful Display —
                // callers see `<iter array 0/3>` rather than a
                // bare `<iter>`. If the RefCell is already
                // borrowed (nested Display during a panic
                // backtrace, say), fall back to the bare form.
                match it.try_borrow() {
                    Ok(inner) => match &*inner {
                        BopIter::Array { items, pos } => {
                            write!(f, "<iter array {}/{}>", pos, items.len())
                        }
                        BopIter::String { chars, pos } => {
                            write!(f, "<iter string {}/{}>", pos, chars.len())
                        }
                        BopIter::Dict { keys, pos } => {
                            write!(f, "<iter dict {}/{}>", pos, keys.len())
                        }
                    },
                    Err(_) => write!(f, "<iter>"),
                }
            }
            Value::Struct(s) => {
                write!(f, "{} {{", s.type_name)?;
                for (i, (k, v)) in s.fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, " {}: {}", k, v.inspect())?;
                }
                if !s.fields.is_empty() {
                    write!(f, " ")?;
                }
                write!(f, "}}")
            }
            Value::EnumVariant(e) => match &e.payload {
                EnumPayload::Unit => write!(f, "{}::{}", e.type_name, e.variant),
                EnumPayload::Tuple(items) => {
                    write!(f, "{}::{}(", e.type_name, e.variant)?;
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", v.inspect())?;
                    }
                    write!(f, ")")
                }
                EnumPayload::Struct(fields) => {
                    write!(f, "{}::{} {{", e.type_name, e.variant)?;
                    for (i, (k, v)) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ",")?;
                        }
                        write!(f, " {}: {}", k, v.inspect())?;
                    }
                    if !fields.is_empty() {
                        write!(f, " ")?;
                    }
                    write!(f, "}}")
                }
            },
        }
    }
}

impl core::fmt::Display for BopStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── Value helpers ─────────────────────────────────────────────────────────

impl Value {
    pub fn inspect(&self) -> String {
        match self {
            Value::Str(s) => format!("\"{}\"", s.0),
            other => format!("{}", other),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Number(_) => "number",
            Value::Str(_) => "string",
            Value::Bool(_) => "bool",
            Value::None => "none",
            Value::Array(_) => "array",
            Value::Dict(_) => "dict",
            Value::Fn(_) => "fn",
            // Generic bucket — the *specific* type name lives on
            // the value itself (`struct_type_name()`). `type()`
            // returns the Bop type name via the display path, so
            // `type(Point { ... })` shows `"Point"`.
            Value::Struct(_) => "struct",
            Value::EnumVariant(_) => "enum",
            Value::Module(_) => "module",
            Value::Iter(_) => "iter",
        }
    }

    /// The user-facing name for this value's type. For struct
    /// values it's the declared type (`"Point"`); for enum
    /// variants it's the enum's type name; for built-in
    /// variants it matches [`Self::type_name`].
    pub fn display_type_name(&self) -> String {
        match self {
            Value::Struct(s) => s.type_name.clone(),
            Value::EnumVariant(e) => e.type_name.clone(),
            other => other.type_name().to_string(),
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::None => false,
            Value::Int(n) => *n != 0,
            Value::Number(n) => *n != 0.0,
            Value::Str(s) => !s.0.is_empty(),
            Value::Array(a) => !a.0.is_empty(),
            Value::Dict(d) => !d.0.is_empty(),
            // A callable is always a "thing" — match other
            // non-empty runtime objects.
            Value::Fn(_) => true,
            // Structs carry fielded data and are always truthy,
            // even if they have no fields (the "unit struct"
            // use case) — matching how classes / records behave
            // in most scripting languages.
            Value::Struct(_) => true,
            // Enum variants represent a tagged choice; always
            // truthy regardless of payload.
            Value::EnumVariant(_) => true,
            // A module is always a concrete thing — matches
            // fn's behaviour.
            Value::Module(_) => true,
            // Iterators are always truthy, even when exhausted.
            // Callers check `Iter::Done` via `.next()`, not via
            // truthiness — matches how fns / modules behave.
            Value::Iter(_) => true,
        }
    }
}

// ─── Deref for read access ─────────────────────────────────────────────────

impl BopStr {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::ops::Deref for BopStr {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl core::ops::Deref for BopArray {
    type Target = [Value];
    fn deref(&self) -> &[Value] {
        &self.0
    }
}

impl core::ops::Deref for BopDict {
    type Target = [(String, Value)];
    fn deref(&self) -> &[(String, Value)] {
        &self.0
    }
}

// ─── Mutation methods ──────────────────────────────────────────────────────

impl BopArray {
    /// Take the inner Vec, leaving an empty array. Deallocates the buffer
    /// from the memory tracker since it's leaving Value's control.
    pub fn take(&mut self) -> Vec<Value> {
        let taken = core::mem::take(&mut self.0);
        bop_dealloc(taken.capacity() * core::mem::size_of::<Value>());
        taken
    }

    /// Set a value at the given index. The old value at that index is dropped
    /// (firing its Drop impl which calls bop_dealloc). No capacity change.
    pub fn set(&mut self, index: usize, val: Value) {
        self.0[index] = val;
    }
}

impl BopStruct {
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Module this struct type was declared in. Forms one half
    /// of the type's identity — the other half is the bare
    /// [`Self::type_name`].
    pub fn module_path(&self) -> &str {
        &self.module_path
    }

    pub fn fields(&self) -> &[(String, Value)] {
        &self.fields
    }

    /// Look up a field by name. `None` if no such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Replace the value of an existing field. Returns `true` if
    /// the field was present; `false` if the caller should raise
    /// a "no such field" error. The old value is dropped (firing
    /// its allocation tracking); no capacity change in the Vec.
    pub fn set_field(&mut self, name: &str, value: Value) -> bool {
        if let Some(entry) = self.fields.iter_mut().find(|(k, _)| k == name) {
            entry.1 = value;
            true
        } else {
            false
        }
    }
}

impl BopEnumVariant {
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Module this enum type was declared in. Paired with
    /// [`Self::type_name`] to form the type's identity.
    pub fn module_path(&self) -> &str {
        &self.module_path
    }

    pub fn variant(&self) -> &str {
        &self.variant
    }

    pub fn payload(&self) -> &EnumPayload {
        &self.payload
    }

    /// Field access for struct-variant payloads — mirrors
    /// [`BopStruct::field`]. Returns `None` for unit / tuple
    /// variants or when the field isn't in this variant's
    /// payload.
    pub fn field(&self, name: &str) -> Option<&Value> {
        match &self.payload {
            EnumPayload::Struct(fields) => {
                fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
            }
            _ => None,
        }
    }
}

impl BopDict {
    /// Set a key-value pair. If the key exists, replaces the value.
    /// If new, tracks the key's allocation and any Vec capacity growth
    /// from the push (Vec may reallocate to a larger buffer).
    pub fn set_key(&mut self, key: &str, val: Value) {
        if let Some(entry) = self.0.iter_mut().find(|(k, _)| k == key) {
            entry.1 = val;
        } else {
            let old_cap = self.0.capacity();
            let key = key.to_string();
            bop_alloc(key.capacity());
            self.0.push((key, val));
            let new_cap = self.0.capacity();
            if new_cap > old_cap {
                bop_alloc((new_cap - old_cap) * core::mem::size_of::<(String, Value)>());
            }
        }
    }
}

// ─── Equality ──────────────────────────────────────────────────────────────

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        // Cross-type numeric equality: `1 == 1.0` is true, same
        // as Python / JS. Widens the Int through f64 for the
        // comparison — lossy for magnitudes above 2^53, but
        // that's the cost of the convenience. Stricter-typed
        // code can call `int()` / `float()` explicitly first.
        (Value::Int(x), Value::Number(y)) => (*x as f64) == *y,
        (Value::Number(x), Value::Int(y)) => *x == (*y as f64),
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::None, Value::None) => true,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b))
        }
        (Value::Dict(x), Value::Dict(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| {
                    y.iter()
                        .find(|(k2, _)| k2 == k)
                        .is_some_and(|(_, v2)| values_equal(v, v2))
                })
        }
        // Functions have identity-based equality: two references
        // to the same `BopFn` compare equal; structurally identical
        // closures constructed independently do not.
        (Value::Fn(a), Value::Fn(b)) => Rc::ptr_eq(a, b),
        // Structural equality for user structs: full type
        // identity (module_path + type_name) AND every field
        // equal in declaration order. Two structs with the same
        // name declared in different modules deliberately compare
        // as *not equal* — they're distinct types.
        (Value::Struct(a), Value::Struct(b)) => {
            a.module_path == b.module_path
                && a.type_name == b.type_name
                && a.fields.len() == b.fields.len()
                && a.fields
                    .iter()
                    .zip(b.fields.iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && values_equal(va, vb))
        }
        // Enum variants: same full type identity (module_path +
        // type_name), same variant name, same payload shape +
        // structural equality on payload items.
        (Value::EnumVariant(a), Value::EnumVariant(b)) => {
            a.module_path == b.module_path
                && a.type_name == b.type_name
                && a.variant == b.variant
                && match (&a.payload, &b.payload) {
                    (EnumPayload::Unit, EnumPayload::Unit) => true,
                    (EnumPayload::Tuple(ax), EnumPayload::Tuple(bx)) => {
                        ax.len() == bx.len()
                            && ax
                                .iter()
                                .zip(bx.iter())
                                .all(|(x, y)| values_equal(x, y))
                    }
                    (EnumPayload::Struct(af), EnumPayload::Struct(bf)) => {
                        af.len() == bf.len()
                            && af
                                .iter()
                                .zip(bf.iter())
                                .all(|((ka, va), (kb, vb))| {
                                    ka == kb && values_equal(va, vb)
                                })
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}
