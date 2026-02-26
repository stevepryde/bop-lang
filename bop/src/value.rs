//! Value type for the Bop interpreter.
//!
//! Heap-allocating variants use newtypes with private fields.
//! The only way to construct them is through the tracked constructors
//! (`Value::new_str`, `Value::new_array`, `Value::new_dict`), which
//! call `bop_alloc`. This is enforced by the type system — code outside
//! this module cannot access the private inner fields.

#[cfg(not(feature = "std"))]
use alloc::{format, string::{String, ToString}, vec::Vec};

use crate::memory::{bop_alloc, bop_dealloc};

// ─── Tracked newtypes ──────────────────────────────────────────────────────
//
// Private inner fields prevent direct construction from outside this module.

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BopStr(String);

#[derive(Debug)]
pub struct BopArray(Vec<Value>);

#[derive(Debug)]
pub struct BopDict(Vec<(String, Value)>);

// ─── Value enum ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum Value {
    Number(f64),
    Str(BopStr),
    Bool(bool),
    None,
    Array(BopArray),
    Dict(BopDict),
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
        _ => false,
    }
}
