//! Function-local slot allocation and lexical visibility tracking.
//!
//! Slot indices grow monotonically for the whole function, while each lexical
//! scope records a restoration journal. The journal owns declaration order;
//! the visibility map is only a compiler-time lookup accelerator. Default
//! builds use randomized hashing for expected constant-time lookup on
//! untrusted identifiers. True `no_std` builds use a dependency-free ordered
//! map with logarithmic lookup.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::BTreeMap as VisibilityMap,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};
#[cfg(test)]
use core::cell::Cell;
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{
    collections::HashMap as VisibilityMap,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use bop::parser::{ParamMode, Parameter};

use crate::chunk::SlotIdx;

#[derive(Default)]
struct VisibleSlots {
    slots: VisibilityMap<Rc<str>, SlotIdx>,
    #[cfg(test)]
    lookup_count: Cell<usize>,
}

impl VisibleSlots {
    fn get(&self, name: &str) -> Option<SlotIdx> {
        #[cfg(test)]
        self.lookup_count.set(self.lookup_count.get() + 1);
        self.slots.get(name).copied()
    }

    fn insert(&mut self, name: Rc<str>, slot: SlotIdx) -> Option<SlotIdx> {
        self.slots.insert(name, slot)
    }

    fn remove(&mut self, name: &str) -> Option<SlotIdx> {
        self.slots.remove(name)
    }

    #[cfg(test)]
    fn reset_lookup_count(&self) {
        self.lookup_count.set(0);
    }

    #[cfg(test)]
    fn lookup_count(&self) -> usize {
        self.lookup_count.get()
    }
}

struct ScopeBinding {
    name: Rc<str>,
    slot: SlotIdx,
    previous: Option<SlotIdx>,
}

#[derive(Default)]
struct LocalScope {
    /// Replaying these entries in reverse restores outer bindings, including
    /// repeated declarations at one lexical depth.
    bindings: Vec<ScopeBinding>,
}

pub(super) struct LocalResolver {
    /// The journals own source order; `visible` must never influence emission.
    scopes: Vec<LocalScope>,
    visible: VisibleSlots,
    /// Slot numbers never roll back when a lexical scope exits.
    next_slot: u32,
    /// High-water mark used to size the VM frame once at call time.
    pub(super) max_slot: u32,
    pub(super) ref_param_slots: Vec<SlotIdx>,
}

impl LocalResolver {
    pub(super) fn new(params: &[Parameter]) -> Self {
        let mut resolver = Self {
            scopes: vec![LocalScope {
                bindings: Vec::with_capacity(params.len()),
            }],
            visible: VisibleSlots::default(),
            next_slot: 0,
            max_slot: 0,
            ref_param_slots: Vec::new(),
        };
        let mut ref_param_slots = Vec::new();
        for parameter in params {
            let slot = SlotIdx(resolver.next_slot);
            resolver.bind_slot(&parameter.name, slot);
            if parameter.mode == ParamMode::Ref {
                ref_param_slots.push(slot);
            }
            resolver.next_slot += 1;
        }
        resolver.max_slot = resolver.next_slot;
        resolver.ref_param_slots = ref_param_slots;
        resolver
    }

    fn bind_slot(&mut self, name: &str, slot: SlotIdx) {
        let name: Rc<str> = Rc::from(name);
        let previous = self.visible.insert(Rc::clone(&name), slot);
        self.scopes
            .last_mut()
            .expect("resolver always has a scope")
            .bindings
            .push(ScopeBinding {
                name,
                slot,
                previous,
            });
    }

    /// Allocate a fresh slot in the innermost scope. A repeated declaration
    /// receives a fresh slot and becomes the visible same-scope winner.
    pub(super) fn declare(&mut self, name: &str) -> SlotIdx {
        let slot = SlotIdx(self.next_slot);
        self.next_slot += 1;
        self.max_slot = self.max_slot.max(self.next_slot);
        self.bind_slot(name, slot);
        slot
    }

    /// Returns `None` for captures, imports, named functions, and builtins so
    /// the compiler can retain its existing name-based fallback.
    pub(super) fn resolve(&self, name: &str) -> Option<SlotIdx> {
        self.visible.get(name)
    }

    /// Return only this scope's names, sorted and deduplicated for deterministic
    /// import-clash metadata.
    pub(super) fn innermost_scope_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .scopes
            .last()
            .map(|scope| {
                scope
                    .bindings
                    .iter()
                    .map(|binding| binding.name.to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names.dedup();
        names
    }

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(LocalScope::default());
    }

    pub(super) fn pop_scope(&mut self) {
        assert!(
            self.scopes.len() > 1,
            "resolver root scope must remain active"
        );
        let scope = self.scopes.pop().expect("scope count checked above");
        for binding in scope.bindings.into_iter().rev() {
            debug_assert_eq!(self.visible.get(&binding.name), Some(binding.slot));
            if let Some(previous) = binding.previous {
                self.visible.insert(binding.name, previous);
            } else {
                self.visible.remove(&binding.name);
            }
        }
        // Reusing a popped slot would require liveness tracking across control
        // flow. Retaining it keeps every local access a direct frame Vec read.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::format;
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    use std::format;

    fn parameter(name: &str, mode: ParamMode) -> Parameter {
        Parameter {
            name: name.to_string(),
            mode,
        }
    }

    #[test]
    fn visibility_index_preserves_redeclaration_shadowing_and_scope_restoration() {
        let params = [
            parameter("outer", ParamMode::Value),
            parameter("by_ref", ParamMode::Ref),
        ];
        let mut resolver = LocalResolver::new(&params);

        assert_eq!(resolver.resolve("outer"), Some(SlotIdx(0)));
        assert_eq!(resolver.resolve("by_ref"), Some(SlotIdx(1)));
        assert_eq!(resolver.ref_param_slots, [SlotIdx(1)]);

        let first = resolver.declare("same_scope");
        let second = resolver.declare("same_scope");
        assert_eq!((first, second), (SlotIdx(2), SlotIdx(3)));
        assert_eq!(resolver.resolve("same_scope"), Some(second));

        resolver.push_scope();
        let inner_outer = resolver.declare("outer");
        resolver.declare("zebra");
        resolver.declare("alpha");
        resolver.declare("zebra");
        assert_eq!(resolver.resolve("outer"), Some(inner_outer));
        assert_eq!(
            resolver.innermost_scope_names(),
            ["alpha", "outer", "zebra"]
        );

        resolver.pop_scope();
        assert_eq!(resolver.resolve("outer"), Some(SlotIdx(0)));
        assert_eq!(resolver.resolve("same_scope"), Some(second));
        assert_eq!(resolver.resolve("missing"), None);
        assert_eq!(resolver.max_slot, 8);
    }

    fn indexed_lookup_count(binding_count: usize) -> usize {
        let mut resolver = LocalResolver::new(&[]);
        for index in 0..binding_count {
            resolver.declare(&format!("binding_{index}"));
        }

        resolver.visible.reset_lookup_count();
        for index in 0..binding_count {
            assert_eq!(
                resolver.resolve(&format!("binding_{index}")),
                Some(SlotIdx(index as u32))
            );
        }
        resolver.visible.lookup_count()
    }

    #[test]
    fn resolution_uses_one_visibility_index_lookup_per_access() {
        let small = indexed_lookup_count(1_024);
        let large = indexed_lookup_count(8_192);

        assert_eq!(small, 1_024);
        assert_eq!(large, 8_192);
        assert_eq!(large / small, 8);
    }
}
