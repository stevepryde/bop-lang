//! Value type for the Bop interpreter.
//!
//! Heap-allocating variants use newtypes with private fields.
//! The only way to construct them is through the tracked constructors
//! (`Value::new_str`, `Value::new_array`, `Value::new_dict`), which
//! call `bop_alloc`. This is enforced by the type system — code outside
//! this module cannot access the private inner fields.

#[cfg(not(feature = "std"))]
use alloc::{format, rc::Rc, string::{String, ToString}, vec::Vec};

#[cfg(feature = "std")]
use std::rc::Rc;

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
    Number(f64),
    Str(BopStr),
    Bool(bool),
    None,
    Array(BopArray),
    Dict(BopDict),
    Fn(Rc<BopFn>),
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
            // Closures are reference-counted: cloning a Value::Fn
            // is O(1) and doesn't duplicate the body or captures.
            // Tracking the captures' memory happens once, at the
            // moment the BopFn is constructed (by `new_fn`), via
            // their own Value Clone/Drop hooks.
            Value::Fn(f) => Value::Fn(Rc::clone(f)),
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
            Value::Number(_) => "number",
            Value::Str(_) => "string",
            Value::Bool(_) => "bool",
            Value::None => "none",
            Value::Array(_) => "array",
            Value::Dict(_) => "dict",
            Value::Fn(_) => "fn",
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::None => false,
            Value::Number(n) => *n != 0.0,
            Value::Str(s) => !s.0.is_empty(),
            Value::Array(a) => !a.0.is_empty(),
            Value::Dict(d) => !d.0.is_empty(),
            // A callable is always a "thing" — match other
            // non-empty runtime objects.
            Value::Fn(_) => true,
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
        (Value::Number(x), Value::Number(y)) => x == y,
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
        _ => false,
    }
}
