//! Value type for the Bop interpreter.
//!
//! Heap-allocating variants use newtypes with private fields.
//! The only way to construct them is through the tracked constructors
//! (`Value::new_str`, `Value::new_array`, `Value::new_dict`), which
//! call `bop_alloc`. This is enforced by the type system — code outside
//! this module cannot access the private inner fields.

#[cfg(feature = "no_std")]
use alloc::{
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};

#[cfg(not(feature = "no_std"))]
use std::rc::Rc;

use core::cell::RefCell;

use crate::error::BopError;
use crate::memory::{bop_alloc, bop_dealloc};
use crate::parser::Stmt;

/// Maximum number of recursively owned runtime values.
///
/// This is deliberately an unconditional runtime invariant rather than a
/// configurable [`crate::BopLimits`] field. `Value`'s `Clone`, `Drop`,
/// `Display`, `Debug`, and equality implementations recurse through owned
/// children, so allowing an embedder (or an unchecked AOT run) to raise the
/// ceiling would re-introduce a native-stack escape from the sandbox.
pub const MAX_VALUE_DEPTH: u16 = 64;

pub const VALUE_DEPTH_ERROR_MESSAGE: &str = "Value nesting limit exceeded (maximum 64 levels)";

fn value_depth_error(line: u32) -> BopError {
    BopError::fatal(VALUE_DEPTH_ERROR_MESSAGE, line)
}

fn checked_owner_depth<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    extra_depth: u16,
    line: u32,
) -> Result<u16, BopError> {
    let child_depth = values
        .into_iter()
        .map(Value::ownership_depth)
        .max()
        .unwrap_or(0);
    let depth = child_depth.saturating_add(extra_depth);
    if depth > MAX_VALUE_DEPTH {
        Err(value_depth_error(line))
    } else {
        Ok(depth)
    }
}

fn trusted<T>(result: Result<T, BopError>) -> T {
    result.unwrap_or_else(|_| panic!("{VALUE_DEPTH_ERROR_MESSAGE}"))
}

// ─── Tracked newtypes ──────────────────────────────────────────────────────
//
// Private inner fields prevent direct construction from outside this module.

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BopStr(String);

#[derive(Debug, Clone)]
pub struct BopArray(Rc<ArrayData>);

#[derive(Debug)]
struct ArrayData {
    items: Vec<Value>,
    depth: u16,
    depth_counts: ArrayDepthCounts,
}

/// Exact child-depth frequencies for an array. Flat values dominate normal
/// programs, so depth zero stays inline; only nested values allocate entries.
/// The sparse vector has at most [`MAX_VALUE_DEPTH`] entries, keeping depth
/// maintenance independent of the array's length without inflating every
/// `Value` by a fixed 64-element table.
#[derive(Debug, Clone)]
struct ArrayDepthCounts {
    flat: usize,
    nested: Vec<(u16, usize)>,
    /// Stable depth recorded when each element enters the array. Re-reading an
    /// iterator's depth can fail conservatively while a host holds its
    /// `RefCell` borrow, so removals and replacements use this parallel cache.
    child_depths: Vec<u16>,
}

impl ArrayDepthCounts {
    fn from_values(values: &[Value], line: u32) -> Result<Self, BopError> {
        let mut counts = Self {
            flat: 0,
            nested: Vec::new(),
            child_depths: Vec::new(),
        };
        counts
            .child_depths
            .try_reserve_exact(values.len())
            .map_err(|_| BopError::fatal("Memory limit exceeded", line))?;
        for value in values {
            let depth = value.ownership_depth();
            counts.child_depths.push(depth);
            if depth == 0 {
                counts.flat += 1;
            } else if let Some((_, count)) = counts
                .nested
                .iter_mut()
                .find(|(entry_depth, _)| *entry_depth == depth)
            {
                *count += 1;
            } else {
                counts
                    .nested
                    .try_reserve(1)
                    .map_err(|_| BopError::fatal("Memory limit exceeded", line))?;
                counts.nested.push((depth, 1));
            }
        }
        Ok(counts)
    }

    fn tracked_bytes(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.nested.capacity() * core::mem::size_of::<(u16, usize)>()
            + self.child_depths.capacity() * core::mem::size_of::<u16>()
    }

    fn ensure_depth(&mut self, depth: u16, line: u32) -> Result<(), BopError> {
        if depth == 0 || self.nested.iter().any(|(entry_depth, _)| *entry_depth == depth) {
            return Ok(());
        }
        let old_capacity = self.nested.capacity();
        self.nested
            .try_reserve(1)
            .map_err(|_| BopError::fatal("Memory limit exceeded", line))?;
        let new_capacity = self.nested.capacity();
        if new_capacity > old_capacity {
            bop_alloc((new_capacity - old_capacity) * core::mem::size_of::<(u16, usize)>());
        }
        Ok(())
    }

    fn add(&mut self, depth: u16) {
        if depth == 0 {
            self.flat += 1;
        } else if let Some((_, count)) = self
            .nested
            .iter_mut()
            .find(|(entry_depth, _)| *entry_depth == depth)
        {
            *count += 1;
        } else {
            // `ensure_depth` reserves this slot before any fallible mutation.
            self.nested.push((depth, 1));
        }
    }

    fn try_reserve_child(&mut self, line: u32) -> Result<(), BopError> {
        let old_capacity = self.child_depths.capacity();
        self.child_depths
            .try_reserve(1)
            .map_err(|_| BopError::fatal("Memory limit exceeded", line))?;
        let new_capacity = self.child_depths.capacity();
        if new_capacity > old_capacity {
            bop_alloc((new_capacity - old_capacity) * core::mem::size_of::<u16>());
        }
        Ok(())
    }

    fn remove(&mut self, depth: u16) {
        if depth == 0 {
            self.flat -= 1;
            return;
        }
        let index = self
            .nested
            .iter()
            .position(|(entry_depth, _)| *entry_depth == depth)
            .expect("array depth metadata contains every child");
        if self.nested[index].1 == 1 {
            self.nested.remove(index);
        } else {
            self.nested[index].1 -= 1;
        }
    }

    fn owner_depth(&self) -> u16 {
        self.nested
            .iter()
            .map(|(depth, _)| depth.saturating_add(1))
            .max()
            .unwrap_or(1)
    }

    fn clear(&mut self) {
        self.flat = 0;
        self.nested.clear();
        self.child_depths.clear();
    }
}

#[derive(Debug, Clone)]
pub struct BopDict(Rc<DictData>);

#[derive(Debug)]
struct DictData {
    entries: Vec<(String, Value)>,
    depth: u16,
}

/// A user-defined struct value. Carries the module it was
/// declared in plus the bare type name, so two modules that
/// happen to declare `struct Color { ... }` independently
/// produce distinct values even when they share a name. The
/// module path is `<root>` for the top-level program and
/// `<builtin>` for engine-registered builtins like
/// `RuntimeError`; for user modules it's the dot-joined `use`
/// path (`"std.math"`, `"game.entity"`, …). Fields are stored
/// in declaration order so iteration and `Display` stay stable.
#[derive(Debug, Clone)]
pub struct BopStruct(Rc<StructData>);

#[derive(Debug)]
struct StructData {
    module_path: String,
    type_name: String,
    fields: Vec<(String, Value)>,
    depth: u16,
}

/// A user-defined enum variant value — the concrete data side of
/// Bop's sum types. Like [`BopStruct`], it's identified by the
/// `(module_path, type_name)` pair, plus the selected variant's
/// name and payload. Two enums declared in different modules with
/// the same type name and even the same variants still compare
/// as distinct types.
#[derive(Debug, Clone)]
pub struct BopEnumVariant(Rc<EnumVariantData>);

#[derive(Debug)]
struct EnumVariantData {
    module_path: String,
    type_name: String,
    variant: String,
    payload: EnumPayload,
    depth: u16,
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
#[derive(Debug, Clone)]
pub enum EnumPayload {
    Unit,
    Tuple(Vec<Value>),
    Struct(Vec<(String, Value)>),
}

impl ArrayData {
    fn tracked_bytes(&self) -> usize {
        self.items.capacity() * core::mem::size_of::<Value>()
            + self.depth_counts.tracked_bytes()
    }
}

impl Clone for ArrayData {
    fn clone(&self) -> Self {
        let cloned = Self {
            items: self.items.clone(),
            depth: self.depth,
            depth_counts: self.depth_counts.clone(),
        };
        bop_alloc(cloned.tracked_bytes());
        cloned
    }
}

impl Drop for ArrayData {
    fn drop(&mut self) {
        bop_dealloc(self.tracked_bytes());
    }
}

impl DictData {
    fn tracked_bytes(&self) -> usize {
        let key_bytes: usize = self.entries.iter().map(|(key, _)| key.capacity()).sum();
        self.entries.capacity() * core::mem::size_of::<(String, Value)>() + key_bytes
    }
}

impl Clone for DictData {
    fn clone(&self) -> Self {
        let cloned = Self {
            entries: self.entries.clone(),
            depth: self.depth,
        };
        bop_alloc(cloned.tracked_bytes());
        cloned
    }
}

impl Drop for DictData {
    fn drop(&mut self) {
        bop_dealloc(self.tracked_bytes());
    }
}

impl StructData {
    fn tracked_bytes(&self) -> usize {
        let key_bytes: usize = self.fields.iter().map(|(key, _)| key.capacity()).sum();
        self.module_path.capacity()
            + self.type_name.capacity()
            + self.fields.capacity() * core::mem::size_of::<(String, Value)>()
            + key_bytes
    }
}

impl Clone for StructData {
    fn clone(&self) -> Self {
        let cloned = Self {
            module_path: self.module_path.clone(),
            type_name: self.type_name.clone(),
            fields: self.fields.clone(),
            depth: self.depth,
        };
        bop_alloc(cloned.tracked_bytes());
        cloned
    }
}

impl Drop for StructData {
    fn drop(&mut self) {
        bop_dealloc(self.tracked_bytes());
    }
}

impl EnumVariantData {
    fn tracked_bytes(&self) -> usize {
        let base = self.module_path.capacity() + self.type_name.capacity() + self.variant.capacity();
        match &self.payload {
            EnumPayload::Unit => base,
            EnumPayload::Tuple(items) => {
                base + items.capacity() * core::mem::size_of::<Value>()
            }
            EnumPayload::Struct(fields) => {
                let key_bytes: usize = fields.iter().map(|(key, _)| key.capacity()).sum();
                base
                    + fields.capacity() * core::mem::size_of::<(String, Value)>()
                    + key_bytes
            }
        }
    }
}

impl Clone for EnumVariantData {
    fn clone(&self) -> Self {
        let cloned = Self {
            module_path: self.module_path.clone(),
            type_name: self.type_name.clone(),
            variant: self.variant.clone(),
            payload: self.payload.clone(),
            depth: self.depth,
        };
        bop_alloc(cloned.tracked_bytes());
        cloned
    }
}

impl Drop for EnumVariantData {
    fn drop(&mut self) {
        bop_dealloc(self.tracked_bytes());
    }
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
    /// Maximum recursive ownership depth, including captures and any
    /// engine-owned values hidden inside an opaque compiled body.
    depth: u16,
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

impl BopFn {
    /// Build an AST-backed function object for engine-internal dispatch paths
    /// that need the `Rc<BopFn>` directly rather than a wrapping [`Value`].
    pub fn try_new_ast(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        line: u32,
    ) -> Result<Rc<Self>, BopError> {
        let depth = checked_owner_depth(captures.iter().map(|(_, value)| value), 1, line)?;
        Ok(Rc::new(Self {
            params,
            captures,
            body: FnBody::Ast(body),
            self_name,
            depth,
        }))
    }

    pub fn try_new_compiled(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Rc<Self>, BopError> {
        let capture_depth = captures
            .iter()
            .map(|(_, value)| value.ownership_depth())
            .max()
            .unwrap_or(0);
        let depth = capture_depth.max(opaque_body_depth).saturating_add(1);
        if depth > MAX_VALUE_DEPTH {
            return Err(value_depth_error(line));
        }
        Ok(Rc::new(Self {
            params,
            captures,
            body: FnBody::Compiled(body),
            self_name,
            depth,
        }))
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
    // Composite user values are small CoW handles. Their backing data stays
    // behind `Rc`, keeping `Value` compact while assignment, argument passing,
    // capture, and return only bump a reference count.
    Struct(BopStruct),
    EnumVariant(BopEnumVariant),
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
pub struct BopIter {
    kind: BopIterKind,
    depth: u16,
}

#[derive(Debug)]
enum BopIterKind {
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
        match &mut self.kind {
            BopIterKind::Array { items, pos } => {
                if *pos < items.len() {
                    let v = items[*pos].clone();
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            BopIterKind::String { chars, pos } => {
                if *pos < chars.len() {
                    let v = Value::new_str(chars[*pos].to_string());
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            BopIterKind::Dict { keys, pos } => {
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
        match &self.kind {
            BopIterKind::Array { items, .. } => {
                bop_dealloc(items.capacity() * core::mem::size_of::<Value>());
            }
            BopIterKind::String { chars, .. } => {
                bop_dealloc(chars.capacity() * core::mem::size_of::<char>());
            }
            BopIterKind::Dict { keys, .. } => {
                let key_bytes: usize = keys.iter().map(|k| k.capacity()).sum();
                bop_dealloc(keys.capacity() * core::mem::size_of::<String>() + key_bytes);
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
    depth: u16,
}

impl BopModule {
    /// Construct a shared module object for engines that also retain it in an
    /// alias table outside the wrapping [`Value::Module`].
    pub fn try_new(
        path: String,
        bindings: Vec<(String, Value)>,
        types: Vec<String>,
        line: u32,
    ) -> Result<Rc<Self>, BopError> {
        let depth = checked_owner_depth(bindings.iter().map(|(_, value)| value), 1, line)?;
        Ok(Rc::new(Self {
            path,
            bindings,
            types,
            depth,
        }))
    }
}

// ─── Tracked constructors ──────────────────────────────────────────────────
//
// These call bop_alloc() to track the allocation but do NOT check the limit.
// Enforcement happens at tick() via bop_memory_exceeded(). This means a single
// operation can overshoot the limit before the next tick catches it. High-risk
// operations (string repeat, string/array concat) use bop_would_exceed() as a
// preflight check in the evaluator to avoid this.

impl Value {
    /// Recursive ownership depth used to keep all `Value` trait operations
    /// within a known-safe native stack bound.
    pub fn ownership_depth(&self) -> u16 {
        match self {
            Value::Array(value) => value.0.depth,
            Value::Dict(value) => value.0.depth,
            Value::Fn(value) => value.depth,
            Value::Struct(value) => value.0.depth,
            Value::EnumVariant(value) => value.0.depth,
            Value::Module(value) => value.depth,
            // A host may ask for the depth while it holds a mutable iterator
            // borrow. Treat that conservatively as already-at-the-limit
            // rather than panicking through `RefCell::borrow`.
            Value::Iter(value) => value
                .try_borrow()
                .map(|iter| iter.depth)
                .unwrap_or(MAX_VALUE_DEPTH),
            _ => 0,
        }
    }

    pub fn new_str(s: String) -> Self {
        bop_alloc(s.capacity());
        Value::Str(BopStr(s))
    }

    /// Trusted compatibility constructor. Runtime engines must use
    /// [`Self::try_new_array`] so a source line can be attached to a clean
    /// fatal diagnostic.
    pub fn new_array(items: Vec<Value>) -> Self {
        trusted(Self::try_new_array(items, 0))
    }

    pub fn try_new_array(items: Vec<Value>, line: u32) -> Result<Self, BopError> {
        let depth = checked_owner_depth(&items, 1, line)?;
        let depth_counts = ArrayDepthCounts::from_values(&items, line)?;
        let data = ArrayData {
            items,
            depth,
            depth_counts,
        };
        bop_alloc(data.tracked_bytes());
        Ok(Value::Array(BopArray(Rc::new(data))))
    }

    pub fn new_dict(entries: Vec<(String, Value)>) -> Self {
        trusted(Self::try_new_dict(entries, 0))
    }

    pub fn try_new_dict(entries: Vec<(String, Value)>, line: u32) -> Result<Self, BopError> {
        let depth = checked_owner_depth(entries.iter().map(|(_, value)| value), 1, line)?;
        let data = DictData { entries, depth };
        bop_alloc(data.tracked_bytes());
        Ok(Value::Dict(BopDict(Rc::new(data))))
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
        trusted(Self::try_new_struct(module_path, type_name, fields, 0))
    }

    pub fn try_new_struct(
        module_path: String,
        type_name: String,
        fields: Vec<(String, Value)>,
        line: u32,
    ) -> Result<Self, BopError> {
        let depth = checked_owner_depth(fields.iter().map(|(_, value)| value), 1, line)?;
        let data = StructData {
            module_path,
            type_name,
            fields,
            depth,
        };
        bop_alloc(data.tracked_bytes());
        Ok(Value::Struct(BopStruct(Rc::new(data))))
    }

    /// Build a built-in iterator that yields each item of
    /// `items` in order. Cloning the returned `Value::Iter`
    /// shares the iteration cursor, so `let b = a; a.next()`
    /// advances `b` too.
    pub fn new_array_iter(items: Vec<Value>) -> Self {
        trusted(Self::try_new_array_iter(items, 0))
    }

    pub fn try_new_array_iter(items: Vec<Value>, line: u32) -> Result<Self, BopError> {
        let depth = checked_owner_depth(&items, 1, line)?;
        bop_alloc(items.capacity() * core::mem::size_of::<Value>());
        Ok(Value::Iter(Rc::new(RefCell::new(BopIter {
            kind: BopIterKind::Array { items, pos: 0 },
            depth,
        }))))
    }

    /// Build a built-in iterator over a string's Unicode code
    /// points.
    pub fn new_string_iter(chars: Vec<char>) -> Self {
        bop_alloc(chars.capacity() * core::mem::size_of::<char>());
        Value::Iter(Rc::new(RefCell::new(BopIter {
            kind: BopIterKind::String { chars, pos: 0 },
            depth: 1,
        })))
    }

    /// Build a built-in iterator over a dict's keys (declaration
    /// order).
    pub fn new_dict_iter(keys: Vec<String>) -> Self {
        let key_bytes: usize = keys.iter().map(|k| k.capacity()).sum();
        bop_alloc(keys.capacity() * core::mem::size_of::<String>() + key_bytes);
        Value::Iter(Rc::new(RefCell::new(BopIter {
            kind: BopIterKind::Dict { keys, pos: 0 },
            depth: 1,
        })))
    }

    pub fn new_enum_unit(module_path: String, type_name: String, variant: String) -> Self {
        let data = EnumVariantData {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Unit,
            depth: 1,
        };
        bop_alloc(data.tracked_bytes());
        Value::EnumVariant(BopEnumVariant(Rc::new(data)))
    }

    pub fn new_enum_tuple(
        module_path: String,
        type_name: String,
        variant: String,
        items: Vec<Value>,
    ) -> Self {
        trusted(Self::try_new_enum_tuple(
            module_path,
            type_name,
            variant,
            items,
            0,
        ))
    }

    pub fn try_new_enum_tuple(
        module_path: String,
        type_name: String,
        variant: String,
        items: Vec<Value>,
        line: u32,
    ) -> Result<Self, BopError> {
        let depth = checked_owner_depth(&items, 1, line)?;
        let data = EnumVariantData {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Tuple(items),
            depth,
        };
        bop_alloc(data.tracked_bytes());
        Ok(Value::EnumVariant(BopEnumVariant(Rc::new(data))))
    }

    pub fn new_enum_struct(
        module_path: String,
        type_name: String,
        variant: String,
        fields: Vec<(String, Value)>,
    ) -> Self {
        trusted(Self::try_new_enum_struct(
            module_path,
            type_name,
            variant,
            fields,
            0,
        ))
    }

    pub fn try_new_enum_struct(
        module_path: String,
        type_name: String,
        variant: String,
        fields: Vec<(String, Value)>,
        line: u32,
    ) -> Result<Self, BopError> {
        let depth = checked_owner_depth(fields.iter().map(|(_, value)| value), 1, line)?;
        let data = EnumVariantData {
            module_path,
            type_name,
            variant,
            payload: EnumPayload::Struct(fields),
            depth,
        };
        bop_alloc(data.tracked_bytes());
        Ok(Value::EnumVariant(BopEnumVariant(Rc::new(data))))
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
        trusted(Self::try_new_fn(params, captures, body, self_name, 0))
    }

    pub fn try_new_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Vec<Stmt>,
        self_name: Option<String>,
        line: u32,
    ) -> Result<Self, BopError> {
        BopFn::try_new_ast(params, captures, body, self_name, line).map(Value::Fn)
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
        trusted(Self::try_new_compiled_fn(
            params, captures, body, self_name, 0, 0,
        ))
    }

    /// Fallible compiled-function constructor. `opaque_body_depth` is the
    /// maximum depth of `Value`s owned inside the engine-specific body but
    /// hidden behind `dyn Any` (notably AOT Rust closure captures).
    pub fn try_new_compiled_fn(
        params: Vec<String>,
        captures: Vec<(String, Value)>,
        body: Rc<dyn core::any::Any + 'static>,
        self_name: Option<String>,
        opaque_body_depth: u16,
        line: u32,
    ) -> Result<Self, BopError> {
        BopFn::try_new_compiled(params, captures, body, self_name, opaque_body_depth, line)
            .map(Value::Fn)
    }

    /// Build a namespace value while accounting for recursively owned
    /// exported bindings.
    pub fn new_module(
        path: String,
        bindings: Vec<(String, Value)>,
        types: Vec<String>,
        line: u32,
    ) -> Result<Self, BopError> {
        BopModule::try_new(path, bindings, types, line).map(Value::Module)
    }
}

// ─── Clone (tracks allocations) ────────────────────────────────────────────
//
// Composite containers are CoW handles. Cloning them only bumps an `Rc`;
// backing storage is cloned and charged by `Rc::make_mut` on the first shared
// mutation. Strings retain their existing owned-copy behaviour.

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
            Value::Array(arr) => Value::Array(arr.clone()),
            Value::Dict(dict) => Value::Dict(dict.clone()),
            Value::Struct(value) => Value::Struct(value.clone()),
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
            Value::EnumVariant(value) => Value::EnumVariant(value.clone()),
        }
    }
}

// ─── Drop (tracks deallocations) ───────────────────────────────────────────

impl Drop for Value {
    fn drop(&mut self) {
        match self {
            Value::Str(s) => bop_dealloc(s.0.capacity()),
            // Composite values, fns, modules, and iterators release an `Rc`.
            // Their owned backing storage accounts for itself exactly once
            // when the final handle drops.
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
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item.inspect())?;
                }
                write!(f, "]")
            }
            Value::Dict(entries) => {
                write!(f, "{{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
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
                    Ok(inner) => match &inner.kind {
                        BopIterKind::Array { items, pos } => {
                            write!(f, "<iter array {}/{}>", pos, items.len())
                        }
                        BopIterKind::String { chars, pos } => {
                            write!(f, "<iter string {}/{}>", pos, chars.len())
                        }
                        BopIterKind::Dict { keys, pos } => {
                            write!(f, "<iter dict {}/{}>", pos, keys.len())
                        }
                    },
                    Err(_) => write!(f, "<iter>"),
                }
            }
            Value::Struct(s) => {
                write!(f, "{} {{", s.type_name())?;
                for (i, (k, v)) in s.fields().iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, " {}: {}", k, v.inspect())?;
                }
                if !s.fields().is_empty() {
                    write!(f, " ")?;
                }
                write!(f, "}}")
            }
            Value::EnumVariant(e) => match e.payload() {
                EnumPayload::Unit => write!(f, "{}::{}", e.type_name(), e.variant()),
                EnumPayload::Tuple(items) => {
                    write!(f, "{}::{}(", e.type_name(), e.variant())?;
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}", v.inspect())?;
                    }
                    write!(f, ")")
                }
                EnumPayload::Struct(fields) => {
                    write!(f, "{}::{} {{", e.type_name(), e.variant())?;
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
            Value::Struct(s) => s.type_name().to_string(),
            Value::EnumVariant(e) => e.type_name().to_string(),
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
            Value::Array(a) => !a.is_empty(),
            Value::Dict(d) => !d.is_empty(),
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
        &self.0.items
    }
}

impl core::ops::Deref for BopDict {
    type Target = [(String, Value)];
    fn deref(&self) -> &[(String, Value)] {
        &self.0.entries
    }
}

// ─── Mutation methods ──────────────────────────────────────────────────────

impl BopArray {
    /// Take the inner Vec, leaving an empty array. Deallocates the buffer
    /// from the memory tracker since it's leaving Value's control.
    pub fn take(&mut self) -> Vec<Value> {
        let data = Rc::make_mut(&mut self.0);
        let taken = core::mem::take(&mut data.items);
        bop_dealloc(taken.capacity() * core::mem::size_of::<Value>());
        data.depth = 1;
        data.depth_counts.clear();
        taken
    }

    fn check_child_depth(value: &Value, line: u32) -> Result<u16, BopError> {
        let child_depth = value.ownership_depth();
        if child_depth.saturating_add(1) > MAX_VALUE_DEPTH {
            Err(value_depth_error(line))
        } else {
            Ok(child_depth)
        }
    }

    fn try_reserve_item(data: &mut ArrayData, line: u32) -> Result<(), BopError> {
        let old_capacity = data.items.capacity();
        data.items
            .try_reserve(1)
            .map_err(|_| BopError::fatal("Memory limit exceeded", line))?;
        let new_capacity = data.items.capacity();
        if new_capacity > old_capacity {
            bop_alloc((new_capacity - old_capacity) * core::mem::size_of::<Value>());
        }
        Ok(())
    }

    fn refresh_depth(data: &mut ArrayData) {
        data.depth = data.depth_counts.owner_depth();
    }

    /// Append one value without cloning the existing array. Capacity growth is
    /// charged exactly once and all fallible checks happen before insertion.
    pub fn try_push(&mut self, value: Value, line: u32) -> Result<(), BopError> {
        let child_depth = Self::check_child_depth(&value, line)?;
        let data = Rc::make_mut(&mut self.0);
        data.depth_counts.ensure_depth(child_depth, line)?;
        data.depth_counts.try_reserve_child(line)?;
        Self::try_reserve_item(data, line)?;
        data.items.push(value);
        data.depth_counts.child_depths.push(child_depth);
        data.depth_counts.add(child_depth);
        Self::refresh_depth(data);
        Ok(())
    }

    /// Remove and return the final value, if any.
    pub fn pop(&mut self) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        let data = Rc::make_mut(&mut self.0);
        let child_depth = data.depth_counts.child_depths.pop()?;
        let value = data.items.pop()?;
        data.depth_counts.remove(child_depth);
        Self::refresh_depth(data);
        Some(value)
    }

    /// Insert a value at an already-normalized endpoint-inclusive index.
    pub fn try_insert(
        &mut self,
        index: usize,
        value: Value,
        line: u32,
    ) -> Result<(), BopError> {
        let child_depth = Self::check_child_depth(&value, line)?;
        let data = Rc::make_mut(&mut self.0);
        data.depth_counts.ensure_depth(child_depth, line)?;
        data.depth_counts.try_reserve_child(line)?;
        Self::try_reserve_item(data, line)?;
        data.items.insert(index, value);
        data.depth_counts.child_depths.insert(index, child_depth);
        data.depth_counts.add(child_depth);
        Self::refresh_depth(data);
        Ok(())
    }

    /// Remove and return a value at an already-normalized element index.
    pub fn remove(&mut self, index: usize) -> Value {
        let data = Rc::make_mut(&mut self.0);
        let child_depth = data.depth_counts.child_depths.remove(index);
        let value = data.items.remove(index);
        data.depth_counts.remove(child_depth);
        Self::refresh_depth(data);
        value
    }

    pub fn reverse(&mut self) {
        let data = Rc::make_mut(&mut self.0);
        data.items.reverse();
        data.depth_counts.child_depths.reverse();
    }

    pub fn sort_by(&mut self, compare: impl FnMut(&Value, &Value) -> core::cmp::Ordering) {
        let data = Rc::make_mut(&mut self.0);
        let mut compare = compare;
        let mut order: Vec<usize> = (0..data.items.len()).collect();
        order.sort_by(|a, b| compare(&data.items[*a], &data.items[*b]));

        // Convert `new position -> old position` into `old position -> new
        // position`, then apply that permutation to values and cached depths
        // together. This preserves stable sort semantics without re-reading a
        // potentially borrowed iterator's depth.
        let mut target = Vec::with_capacity(order.len());
        target.resize(order.len(), 0usize);
        for (new_position, old_position) in order.into_iter().enumerate() {
            target[old_position] = new_position;
        }
        for position in 0..target.len() {
            while target[position] != position {
                let destination = target[position];
                data.items.swap(position, destination);
                data.depth_counts.child_depths.swap(position, destination);
                target.swap(position, destination);
            }
        }
    }

    /// Set a value at the given index. The old value at that index is dropped
    /// (firing its Drop impl which calls bop_dealloc). No capacity change.
    pub fn set(&mut self, index: usize, val: Value) {
        trusted(self.try_set(index, val, 0));
    }

    /// Line-aware, atomic variant of [`Self::set`]. The existing element is
    /// left untouched if the replacement would exceed [`MAX_VALUE_DEPTH`].
    pub fn try_set(&mut self, index: usize, val: Value, line: u32) -> Result<(), BopError> {
        let new_child_depth = Self::check_child_depth(&val, line)?;
        let old_child_depth = self.0.depth_counts.child_depths[index];
        let data = Rc::make_mut(&mut self.0);
        if new_child_depth != old_child_depth {
            data.depth_counts.ensure_depth(new_child_depth, line)?;
        }
        data.items[index] = val;
        data.depth_counts.child_depths[index] = new_child_depth;
        if new_child_depth != old_child_depth {
            data.depth_counts.remove(old_child_depth);
            data.depth_counts.add(new_child_depth);
            Self::refresh_depth(data);
        }
        Ok(())
    }
}

impl BopStruct {
    pub fn type_name(&self) -> &str {
        &self.0.type_name
    }

    /// Module this struct type was declared in. Forms one half
    /// of the type's identity — the other half is the bare
    /// [`Self::type_name`].
    pub fn module_path(&self) -> &str {
        &self.0.module_path
    }

    pub fn fields(&self) -> &[(String, Value)] {
        &self.0.fields
    }

    /// Look up a field by name. `None` if no such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.0
            .fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// Replace the value of an existing field. Returns `true` if
    /// the field was present; `false` if the caller should raise
    /// a "no such field" error. The old value is dropped (firing
    /// its allocation tracking); no capacity change in the Vec.
    pub fn set_field(&mut self, name: &str, value: Value) -> bool {
        trusted(self.try_set_field(name, value, 0))
    }

    /// Line-aware, atomic variant of [`Self::set_field`].
    pub fn try_set_field(&mut self, name: &str, value: Value, line: u32) -> Result<bool, BopError> {
        let Some(index) = self.0.fields.iter().position(|(key, _)| key == name) else {
            return Ok(false);
        };
        let depth = checked_owner_depth(
            self.0
                .fields
                .iter()
                .enumerate()
                .filter_map(|(i, (_, value))| (i != index).then_some(value))
                .chain(core::iter::once(&value)),
            1,
            line,
        )?;
        let data = Rc::make_mut(&mut self.0);
        data.fields[index].1 = value;
        data.depth = depth;
        Ok(true)
    }
}

impl BopEnumVariant {
    pub fn type_name(&self) -> &str {
        &self.0.type_name
    }

    /// Module this enum type was declared in. Paired with
    /// [`Self::type_name`] to form the type's identity.
    pub fn module_path(&self) -> &str {
        &self.0.module_path
    }

    pub fn variant(&self) -> &str {
        &self.0.variant
    }

    pub fn payload(&self) -> &EnumPayload {
        &self.0.payload
    }

    /// Field access for struct-variant payloads — mirrors
    /// [`BopStruct::field`]. Returns `None` for unit / tuple
    /// variants or when the field isn't in this variant's
    /// payload.
    pub fn field(&self, name: &str) -> Option<&Value> {
        match &self.0.payload {
            EnumPayload::Struct(fields) => fields.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }
}

impl BopDict {
    /// Set a key-value pair. If the key exists, replaces the value.
    /// If new, tracks the key's allocation and any Vec capacity growth
    /// from the push (Vec may reallocate to a larger buffer).
    pub fn set_key(&mut self, key: &str, val: Value) {
        trusted(self.try_set_key(key, val, 0));
    }

    /// Line-aware, atomic variant of [`Self::set_key`].
    pub fn try_set_key(&mut self, key: &str, val: Value, line: u32) -> Result<(), BopError> {
        let existing = self
            .0
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == key);
        let depth = checked_owner_depth(
            self.0
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, (_, value))| (Some(i) != existing).then_some(value))
                .chain(core::iter::once(&val)),
            1,
            line,
        )?;

        let data = Rc::make_mut(&mut self.0);
        if let Some(index) = existing {
            data.entries[index].1 = val;
        } else {
            let old_cap = data.entries.capacity();
            let key = key.to_string();
            bop_alloc(key.capacity());
            data.entries.push((key, val));
            let new_cap = data.entries.capacity();
            if new_cap > old_cap {
                bop_alloc((new_cap - old_cap) * core::mem::size_of::<(String, Value)>());
            }
        }
        data.depth = depth;
        Ok(())
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
            a.module_path() == b.module_path()
                && a.type_name() == b.type_name()
                && a.fields().len() == b.fields().len()
                && a.fields()
                    .iter()
                    .zip(b.fields().iter())
                    .all(|((ka, va), (kb, vb))| ka == kb && values_equal(va, vb))
        }
        // Enum variants: same full type identity (module_path +
        // type_name), same variant name, same payload shape +
        // structural equality on payload items.
        (Value::EnumVariant(a), Value::EnumVariant(b)) => {
            a.module_path() == b.module_path()
                && a.type_name() == b.type_name()
                && a.variant() == b.variant()
                && match (a.payload(), b.payload()) {
                    (EnumPayload::Unit, EnumPayload::Unit) => true,
                    (EnumPayload::Tuple(ax), EnumPayload::Tuple(bx)) => {
                        ax.len() == bx.len()
                            && ax.iter().zip(bx.iter()).all(|(x, y)| values_equal(x, y))
                    }
                    (EnumPayload::Struct(af), EnumPayload::Struct(bf)) => {
                        af.len() == bf.len()
                            && af
                                .iter()
                                .zip(bf.iter())
                                .all(|((ka, va), (kb, vb))| ka == kb && values_equal(va, vb))
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

#[cfg(test)]
mod depth_tests {
    use super::*;
    use crate::memory::{bop_memory_init, bop_memory_used};

    fn nested_array(depth: u16) -> Value {
        let mut value = Value::None;
        for _ in 0..depth {
            value = Value::try_new_array(vec![value], 7).expect("depth within limit");
        }
        value
    }

    fn assert_depth_error(result: Result<Value, BopError>, line: u32) {
        let error = result.expect_err("construction should exceed the value depth limit");
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(line));
        assert_eq!(error.message, VALUE_DEPTH_ERROR_MESSAGE);
    }

    #[test]
    fn maximum_depth_is_safe_for_recursive_value_operations() {
        let value = nested_array(MAX_VALUE_DEPTH);
        assert_eq!(value.ownership_depth(), MAX_VALUE_DEPTH);

        let cloned = value.clone();
        assert!(values_equal(&value, &cloned));
        let displayed = format!("{value}");
        assert_eq!(
            displayed.len(),
            "none".len() + usize::from(MAX_VALUE_DEPTH) * 2
        );
        assert_eq!(value.inspect(), displayed);

        assert_depth_error(Value::try_new_array(vec![value], 19), 19);
        drop(cloned);
    }

    #[test]
    fn every_recursive_owner_enforces_the_same_boundary() {
        let child = nested_array(MAX_VALUE_DEPTH - 1);

        assert_eq!(
            Value::try_new_dict(vec![("x".into(), child.clone())], 1)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_struct("m".into(), "S".into(), vec![("x".into(), child.clone())], 1)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_enum_tuple("m".into(), "E".into(), "V".into(), vec![child.clone()], 1,)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_enum_struct(
                "m".into(),
                "E".into(),
                "V".into(),
                vec![("x".into(), child.clone())],
                1,
            )
            .unwrap()
            .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_fn(
                Vec::new(),
                vec![("x".into(), child.clone())],
                Vec::new(),
                None,
                1,
            )
            .unwrap()
            .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_compiled_fn(
                Vec::new(),
                Vec::new(),
                Rc::new(()),
                None,
                MAX_VALUE_DEPTH - 1,
                1,
            )
            .unwrap()
            .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::new_module("m".into(), vec![("x".into(), child.clone())], Vec::new(), 1,)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );
        assert_eq!(
            Value::try_new_array_iter(vec![child], 1)
                .unwrap()
                .ownership_depth(),
            MAX_VALUE_DEPTH
        );

        assert_depth_error(
            Value::try_new_compiled_fn(
                Vec::new(),
                Vec::new(),
                Rc::new(()),
                None,
                MAX_VALUE_DEPTH,
                23,
            ),
            23,
        );
    }

    #[test]
    fn mutations_reject_atomically() {
        let child = nested_array(MAX_VALUE_DEPTH);

        let mut array = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let Value::Array(array_value) = &mut array else {
            unreachable!()
        };
        let error = array_value.try_set(0, child.clone(), 31).unwrap_err();
        assert!(error.is_fatal);
        assert!(matches!(array_value.first(), Some(Value::Int(1))));
        assert_eq!(array_value.0.depth, 1);

        let mut dict = Value::try_new_dict(vec![("x".into(), Value::Int(1))], 1).unwrap();
        let Value::Dict(dict_value) = &mut dict else {
            unreachable!()
        };
        dict_value.try_set_key("x", child.clone(), 32).unwrap_err();
        assert!(matches!(&dict_value[0].1, Value::Int(1)));
        assert_eq!(dict_value.0.depth, 1);

        let mut structure =
            Value::try_new_struct("m".into(), "S".into(), vec![("x".into(), Value::Int(1))], 1)
                .unwrap();
        let Value::Struct(struct_value) = &mut structure else {
            unreachable!()
        };
        struct_value.try_set_field("x", child, 33).unwrap_err();
        assert!(matches!(struct_value.field("x"), Some(Value::Int(1))));
        assert_eq!(struct_value.0.depth, 1);
    }

    #[test]
    fn array_mutations_keep_exact_sparse_depth_metadata() {
        let mut value = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };

        array.try_push(nested_array(4), 1).unwrap();
        array.try_push(nested_array(2), 1).unwrap();
        assert_eq!(array.0.depth, 5);
        assert_eq!(array.0.depth_counts.flat, 1);
        assert_eq!(array.0.depth_counts.nested.len(), 2);

        array.try_set(1, Value::Int(2), 1).unwrap();
        assert_eq!(array.0.depth, 3);
        assert_eq!(array.0.depth_counts.flat, 2);

        let removed = array.remove(2);
        assert_eq!(removed.ownership_depth(), 2);
        assert_eq!(array.0.depth, 1);
        assert!(array.0.depth_counts.nested.is_empty());

        assert!(matches!(array.pop(), Some(Value::Int(2))));
        assert!(matches!(array.pop(), Some(Value::Int(1))));
        assert!(array.is_empty());
        assert_eq!(array.0.depth, 1);
        assert_eq!(array.0.depth_counts.flat, 0);
    }

    #[test]
    fn array_growth_rejects_deep_values_without_partial_mutation() {
        let child = nested_array(MAX_VALUE_DEPTH);
        let mut value = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };

        let error = array.try_push(child.clone(), 41).unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(41));
        assert_eq!(error.message, VALUE_DEPTH_ERROR_MESSAGE);
        assert_eq!(array.len(), 1);
        assert!(matches!(array.first(), Some(Value::Int(1))));
        assert_eq!(array.0.depth, 1);

        let error = array.try_insert(0, child, 42).unwrap_err();
        assert!(error.is_fatal);
        assert_eq!(error.line, Some(42));
        assert_eq!(array.len(), 1);
        assert!(matches!(array.first(), Some(Value::Int(1))));
        assert_eq!(array.0.depth, 1);
    }

    #[test]
    fn array_mutation_uses_cached_depth_while_iterator_is_borrowed() {
        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_array(vec![iterator], 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        let popped = array.pop().expect("iterator should pop");
        assert!(matches!(popped, Value::Iter(_)));
        assert!(array.is_empty());
        assert_eq!(array.0.depth, 1);
        drop(borrow);

        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_array(vec![Value::Int(0), iterator], 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        let removed = array.remove(1);
        assert!(matches!(removed, Value::Iter(_)));
        assert_eq!(array.len(), 1);
        assert!(matches!(array.first(), Some(Value::Int(0))));
        assert_eq!(array.0.depth, 1);
        drop(borrow);

        let iterator = Value::new_array_iter(vec![Value::Int(1)]);
        let handle = match &iterator {
            Value::Iter(handle) => Rc::clone(handle),
            _ => unreachable!(),
        };
        let mut value = Value::try_new_array(vec![iterator], 1).unwrap();
        let borrow = handle.borrow_mut();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        array.try_set(0, Value::Int(9), 51).unwrap();
        assert!(matches!(array.first(), Some(Value::Int(9))));
        assert_eq!(array.0.depth, 1);
        drop(borrow);
    }

    #[test]
    fn array_reordering_keeps_child_depths_aligned() {
        let mut value = Value::try_new_array(
            vec![nested_array(2), Value::Int(1), nested_array(1)],
            1,
        )
        .unwrap();
        let Value::Array(array) = &mut value else {
            unreachable!()
        };

        array.sort_by(|left, right| left.ownership_depth().cmp(&right.ownership_depth()));
        assert_eq!(array.0.depth_counts.child_depths, [0, 1, 2]);
        assert_eq!(
            array
                .iter()
                .map(Value::ownership_depth)
                .collect::<Vec<_>>(),
            array.0.depth_counts.child_depths
        );

        array.reverse();
        assert_eq!(array.0.depth_counts.child_depths, [2, 1, 0]);
        assert_eq!(
            array
                .iter()
                .map(Value::ownership_depth)
                .collect::<Vec<_>>(),
            array.0.depth_counts.child_depths
        );
    }

    #[test]
    fn array_cow_charges_once_and_unique_pushes_do_not_copy() {
        bop_memory_init(usize::MAX);

        let mut items = Vec::with_capacity(8);
        items.extend([Value::Int(1), Value::Int(2), Value::Int(3)]);
        let mut value = Value::try_new_array(items, 1).unwrap();

        // Leave spare room in both the values buffer and its parallel depth
        // cache, then prove a unique push neither detaches nor allocates.
        let Value::Array(array) = &mut value else {
            unreachable!()
        };
        assert!(matches!(array.pop(), Some(Value::Int(3))));
        let unique_ptr = Rc::as_ptr(&array.0);
        let before_unique_push = bop_memory_used();
        array.try_push(Value::Int(3), 1).unwrap();
        assert_eq!(Rc::as_ptr(&array.0), unique_ptr);
        assert_eq!(bop_memory_used(), before_unique_push);

        // Recreate spare cache capacity before sharing. The first push through
        // the original handle must detach exactly once; the next push remains
        // on that detached allocation.
        assert!(matches!(array.pop(), Some(Value::Int(3))));
        let before_clone = bop_memory_used();
        let snapshot = Value::Array(array.clone());
        assert_eq!(bop_memory_used(), before_clone, "Rc clone must be O(1)");
        let shared_ptr = Rc::as_ptr(&array.0);

        array.try_push(Value::Int(4), 1).unwrap();
        assert_ne!(Rc::as_ptr(&array.0), shared_ptr);
        let detached_usage = bop_memory_used();
        assert_eq!(
            detached_usage - before_clone,
            array.0.tracked_bytes(),
            "shared push must charge one detached backing store"
        );
        assert!(matches!(&snapshot, Value::Array(old) if old.len() == 2));

        let detached_ptr = Rc::as_ptr(&array.0);
        array.try_push(Value::Int(5), 1).unwrap();
        assert_eq!(Rc::as_ptr(&array.0), detached_ptr);
        assert_eq!(bop_memory_used(), detached_usage);

        drop(snapshot);
        assert_eq!(bop_memory_used(), array.0.tracked_bytes());
        drop(value);
        assert_eq!(bop_memory_used(), 0);
    }

    #[test]
    fn dict_and_struct_detach_once_while_enum_clones_stay_shared() {
        bop_memory_init(usize::MAX);

        let mut dict = Value::try_new_dict(vec![("x".into(), Value::Int(1))], 1).unwrap();
        let dict_clone = dict.clone();
        let after_dict_clone = bop_memory_used();
        let Value::Dict(dict_value) = &mut dict else {
            unreachable!()
        };
        let old_dict_ptr = Rc::as_ptr(&dict_value.0);
        dict_value.try_set_key("x", Value::Int(2), 1).unwrap();
        assert_ne!(Rc::as_ptr(&dict_value.0), old_dict_ptr);
        let after_dict_detach = bop_memory_used();
        assert_eq!(
            after_dict_detach - after_dict_clone,
            dict_value.0.tracked_bytes()
        );
        dict_value.try_set_key("x", Value::Int(3), 1).unwrap();
        assert_eq!(bop_memory_used(), after_dict_detach);
        assert!(matches!(&dict_clone, Value::Dict(old) if matches!(&old[0].1, Value::Int(1))));

        let mut structure = Value::try_new_struct(
            "m".into(),
            "S".into(),
            vec![("x".into(), Value::Int(1))],
            1,
        )
        .unwrap();
        let before_struct_clone = bop_memory_used();
        let struct_clone = structure.clone();
        assert_eq!(bop_memory_used(), before_struct_clone);
        let Value::Struct(struct_value) = &mut structure else {
            unreachable!()
        };
        let old_struct_ptr = Rc::as_ptr(&struct_value.0);
        struct_value
            .try_set_field("x", Value::Int(2), 1)
            .unwrap();
        assert_ne!(Rc::as_ptr(&struct_value.0), old_struct_ptr);
        let after_struct_detach = bop_memory_used();
        struct_value
            .try_set_field("x", Value::Int(3), 1)
            .unwrap();
        assert_eq!(bop_memory_used(), after_struct_detach);
        assert!(matches!(struct_clone, Value::Struct(ref old) if matches!(old.field("x"), Some(Value::Int(1)))));

        let variant = Value::try_new_enum_struct(
            "m".into(),
            "E".into(),
            "V".into(),
            vec![("x".into(), Value::Int(1))],
            1,
        )
        .unwrap();
        let before_enum_clone = bop_memory_used();
        let variant_clone = variant.clone();
        assert_eq!(bop_memory_used(), before_enum_clone);
        assert!(matches!(
            (&variant, &variant_clone),
            (Value::EnumVariant(a), Value::EnumVariant(b)) if Rc::ptr_eq(&a.0, &b.0)
        ));

        drop(variant_clone);
        drop(variant);
        drop(struct_clone);
        drop(structure);
        drop(dict_clone);
        drop(dict);
        assert_eq!(bop_memory_used(), 0);
    }

    #[test]
    fn shared_container_dag_releases_storage_with_its_last_owner() {
        bop_memory_init(usize::MAX);

        let child = Value::try_new_array(vec![Value::Int(1)], 1).unwrap();
        let outer = Value::try_new_array(vec![child.clone(), child.clone()], 1).unwrap();
        let used_once = bop_memory_used();
        let outer_clone = outer.clone();
        assert_eq!(bop_memory_used(), used_once);

        drop(child);
        drop(outer);
        assert_eq!(bop_memory_used(), used_once);
        drop(outer_clone);
        assert_eq!(bop_memory_used(), 0);
    }
}
