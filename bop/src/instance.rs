//! Stateful host-callable tree-walker instances and shared ABI metadata.

#[cfg(feature = "no_std")]
use alloc::{format, string::String, vec::Vec};

use crate::builtins::error;
use crate::{BopError, BopHost, BopLimits, ReplSession, Value};
use core::cell::Cell;
use std_or_alloc::rc::Rc;

#[cfg(feature = "no_std")]
use alloc as std_or_alloc;
#[cfg(not(feature = "no_std"))]
use std as std_or_alloc;

use crate::memory::{ActiveMemoryGuard, MemoryAccount};

/// A public root function exposed by a loaded [`crate::BopInstance`].
///
/// Entries are returned in final declaration order. Fields are intentionally
/// private so future ABI metadata can be added without making struct literals
/// a compatibility constraint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryPoint {
    pub(crate) name: String,
    pub(crate) arity: usize,
}

impl EntryPoint {
    #[doc(hidden)]
    pub fn __new(name: String, arity: usize) -> Self {
        Self { name, arity }
    }

    /// Source name of the public root function.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of positional arguments accepted by the entry point.
    pub const fn arity(&self) -> usize {
        self.arity
    }
}

/// A loaded tree-walker program whose globals, imports, functions, types,
/// methods, RNG state, and returned callbacks remain live across calls.
///
/// Hosts are deliberately borrowed only for `load` and each call. An
/// instance never stores a host reference, so callers may use a different
/// compatible host for later operations.
pub struct BopInstance {
    session: ReplSession,
    entries: Vec<EntryPoint>,
    limits: BopLimits,
    in_operation: Cell<bool>,
    memory: Rc<MemoryAccount>,
}

impl BopInstance {
    /// Parse and evaluate a program, retaining its module state for later
    /// calls to root-level `pub fn` declarations.
    pub fn load(
        source: &str,
        host: &mut dyn BopHost,
        limits: &BopLimits,
    ) -> Result<Self, BopError> {
        let stmts = crate::parse(source)?;
        let mut session = ReplSession::new();
        let memory = MemoryAccount::__new(limits.max_memory);
        {
            let _memory = ActiveMemoryGuard::__activate(&memory);
            session.run_stmts(&stmts, host, limits)?;
            if memory.__exceeded() {
                return Err(memory_limit_error());
            }
        }
        let entries = session.instance_entries();
        Ok(Self {
            session,
            entries,
            limits: limits.clone(),
            in_operation: Cell::new(false),
            memory,
        })
    }

    /// Public root functions in final surviving declaration order.
    pub fn entry_points(&self) -> &[EntryPoint] {
        &self.entries
    }

    /// Call a public root function by its dedicated ABI name.
    pub fn call(
        &mut self,
        name: &str,
        args: &[Value],
        host: &mut dyn BopHost,
    ) -> Result<Value, BopError> {
        let entry = self.entries.iter().find(|entry| entry.name == name).ok_or_else(|| {
            error(0, format!("Public entry point `{}` was not found", name))
        })?;
        if args.len() != entry.arity {
            return Err(error(
                0,
                format!(
                    "`{}` expects {} argument{}, but got {}",
                    name,
                    entry.arity,
                    if entry.arity == 1 { "" } else { "s" },
                    args.len(),
                ),
            ));
        }
        let _operation = OperationGuard::begin(&self.in_operation)?;
        if self.memory.__exceeded() {
            return Err(memory_limit_error());
        }
        let _memory = ActiveMemoryGuard::__activate(&self.memory);
        let result = self.session.call_named(name, args, host, &self.limits);
        if self.memory.__exceeded() {
            Err(memory_limit_error())
        } else {
            result
        }
    }

    /// Invoke a callback value created by this instance.
    pub fn call_value(
        &mut self,
        callable: &Value,
        args: &[Value],
        host: &mut dyn BopHost,
    ) -> Result<Value, BopError> {
        self.session.validate_instance_callable(callable, args.len())?;
        let _operation = OperationGuard::begin(&self.in_operation)?;
        if self.memory.__exceeded() {
            return Err(memory_limit_error());
        }
        let _memory = ActiveMemoryGuard::__activate(&self.memory);
        let result = self.session.call_fn(
            callable.clone(),
            args.to_vec(),
            host,
            &self.limits,
        );
        if self.memory.__exceeded() {
            Err(memory_limit_error())
        } else {
            result
        }
    }
}

fn memory_limit_error() -> BopError {
    BopError::fatal("Memory limit exceeded", 0)
}

struct OperationGuard<'a>(&'a Cell<bool>);

impl<'a> OperationGuard<'a> {
    fn begin(flag: &'a Cell<bool>) -> Result<Self, BopError> {
        if flag.replace(true) {
            return Err(error(0, "A Bop instance cannot be re-entered"));
        }
        Ok(Self(flag))
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Host;
    impl BopHost for Host {
        fn call(
            &mut self,
            _name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            None
        }
    }

    #[test]
    fn final_public_declarations_define_the_abi() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "pub fn first() { return 1 }\npub fn gone() {}\nfn gone() {}\npub fn first(x) { return x }\npub fn last() { return 3 }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        let entries: Vec<(&str, usize)> = instance
            .entry_points()
            .iter()
            .map(|entry| (entry.name(), entry.arity()))
            .collect();
        assert_eq!(entries, vec![("first", 1), ("last", 0)]);
        assert_eq!(instance.call("first", &[Value::Int(9)], &mut host).unwrap().inspect(), "9");
        assert_eq!(instance.call("last", &[], &mut host).unwrap().inspect(), "3");
        assert!(instance.call("gone", &[], &mut host).is_err());
    }

    #[test]
    fn calls_retain_and_mutate_root_globals() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "let count = 0\npub fn next() { count += 1; return count }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "1");
        assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "2");
    }

    #[test]
    fn executed_entries_are_dedicated_and_early_return_is_final() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "pub fn entry() { fn entry(x) { return x }; return 1 }\nreturn\npub fn skipped() {}",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.entry_points().len(), 1);
        assert_eq!(instance.call("entry", &[], &mut host).unwrap().inspect(), "1");
        assert_eq!(instance.call("entry", &[], &mut host).unwrap().inspect(), "1");
        assert!(instance.call("skipped", &[], &mut host).is_err());
    }

    #[test]
    fn callbacks_are_bound_to_the_creating_instance() {
        let source = "pub fn make() { return fn() { return 7 } }";
        let mut host = Host;
        let mut first = BopInstance::load(source, &mut host, &BopLimits::standard()).unwrap();
        let callback = first.call("make", &[], &mut host).unwrap();
        assert_eq!(first.call_value(&callback, &[], &mut host).unwrap().inspect(), "7");
        let mut second = BopInstance::load(source, &mut host, &BopLimits::standard()).unwrap();
        let error = second.call_value(&callback, &[], &mut host).unwrap_err();
        assert_eq!(error.line, Some(0));
        assert!(error.message.contains("different Bop engine instance"));
    }

    struct ModuleHost {
        modules: BTreeMap<String, String>,
    }

    impl BopHost for ModuleHost {
        fn call(&mut self, _name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
            None
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn module_globals_are_live_through_aliases_and_facades() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 0\nfn bump() { count += 1; return count }".to_string(),
        );
        modules.insert("facade".to_string(), "use leaf".to_string());
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use facade as f\npub fn next() { let value = f.bump(); return [value, f.count] }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "[1, 1]");
        assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "[2, 2]");
    }

    struct RetainingHost {
        retained: Option<Value>,
    }

    impl BopHost for RetainingHost {
        fn call(&mut self, name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
            if name != "retain_large" {
                return None;
            }
            self.retained = Some(Value::new_str("x".repeat(16 * 1024)));
            Some(Ok(Value::None))
        }
    }

    #[test]
    fn host_allocations_are_suspended_and_final_return_is_checked() {
        let limits = BopLimits { max_steps: 100, max_memory: 32 };
        let mut host = RetainingHost { retained: None };
        let mut instance = BopInstance::load(
            "pub fn host_only() { retain_large() }\npub fn too_large() { return \"abcdefghijklmnopqrstuvwxyz0123456789\" }",
            &mut host,
            &limits,
        )
        .unwrap();
        instance.call("host_only", &[], &mut host).unwrap();
        assert!(host.retained.is_some());
        let error = instance.call("too_large", &[], &mut host).unwrap_err();
        assert!(error.is_fatal);
        assert!(error.message.contains("Memory limit"));
    }
}
