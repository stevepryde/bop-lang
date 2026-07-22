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
        let _operation = OperationGuard::begin(&self.in_operation)?;
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
        let _operation = OperationGuard::begin(&self.in_operation)?;
        self.session.validate_instance_callable(callable, args.len())?;
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
    use std::cell::RefCell;
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
    fn top_level_return_skips_a_stripped_tail_expression() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "pub fn ok() { return 1 }\nreturn\npanic(\"must not run\")",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.call("ok", &[], &mut host).unwrap().inspect(), "1");
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
            "let count = 0\nlet items = [1]\nfn bump() { count += 1; return count }"
                .to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nfn read() { return count }\nfn inc() { count += 1; return count }\nfn add() { items.push(2); return items }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use facade as f\npub fn next() { let before = f.read(); let value = f.inc(); let after = f.read(); let items = f.add(); return [before, value, after, f.count, items, f.items] }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[0, 1, 1, 1, [1, 2], [1, 2]]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, 2, 2, 2, [1, 2, 2], [1, 2, 2]]"
        );
    }

    #[test]
    fn facade_local_overwrite_returns_the_forwarded_value_to_its_origin() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 7\nfn read() { return count }\nfn bump() { count += 1; return count }"
                .to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nlet count = 100\nfn own_bump() { count += 1; return count }".to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use facade as facade\nuse leaf as leaf\npub fn bump() { return facade.own_bump() }\npub fn leaf_read() { return leaf.read() }\npub fn leaf_bump() { return leaf.bump() }\npub fn read() { return [facade.count, leaf.count] }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();

        assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "[100, 7]");
        assert_eq!(instance.call("leaf_read", &[], &mut host).unwrap().inspect(), "7");
        assert_eq!(instance.call("leaf_bump", &[], &mut host).unwrap().inspect(), "8");
        assert_eq!(instance.call("bump", &[], &mut host).unwrap().inspect(), "101");
        assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "[101, 8]");
    }

    #[test]
    fn callbacks_keep_module_globals_live_instead_of_capturing_snapshots() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "let count = 0\npub fn make() { return fn() { count += 1; return count } }\npub fn inc() { count += 1; return count }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        let callback = instance.call("make", &[], &mut host).unwrap();
        assert_eq!(instance.call_value(&callback, &[], &mut host).unwrap().inspect(), "1");
        assert_eq!(instance.call("inc", &[], &mut host).unwrap().inspect(), "2");
        assert_eq!(instance.call_value(&callback, &[], &mut host).unwrap().inspect(), "3");
    }

    #[test]
    fn module_handles_read_the_active_defining_environment() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "state".to_string(),
            "let count = 0\nlet items = [1]\nfn via(handle) { count += 1; items.push(2); return [handle.count, handle.items] }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use state as state\npub fn next() { return state.via(state) }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, [1, 2]]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[2, [1, 2, 2]]"
        );
    }

    #[test]
    fn active_module_handles_fall_back_to_callable_export_metadata() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "state".to_string(),
            "fn helper() { return 7 }\nfn via(handle) { return handle.helper() }".to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use state as state\npub fn call() { return state.via(state) }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.call("call", &[], &mut host).unwrap().inspect(), "7");
    }

    #[test]
    fn failed_facade_initialization_restores_dependency_values() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 0\nfn bump() { count += 1; return count }".to_string(),
        );
        modules.insert("bad".to_string(), "use leaf\nmissing()".to_string());
        modules.insert("bad_signal".to_string(), "use leaf\nbreak".to_string());
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "pub fn fail() { use bad }\npub fn fail_signal() { use bad_signal }\npub fn recover() { use leaf as leaf; return leaf.bump() }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert!(instance.call("fail", &[], &mut host).is_err());
        assert_eq!(
            instance.call("recover", &[], &mut host).unwrap().inspect(),
            "1"
        );
        assert!(instance.call("fail_signal", &[], &mut host).is_err());
        assert_eq!(
            instance.call("recover", &[], &mut host).unwrap().inspect(),
            "2"
        );
    }

    #[test]
    fn facade_handles_read_forwarded_bindings_from_the_active_facade() {
        let mut modules = BTreeMap::new();
        modules.insert(
            "leaf".to_string(),
            "let count = 0\nlet items = [1]".to_string(),
        );
        modules.insert(
            "facade".to_string(),
            "use leaf\nfn via(handle) { count += 1; items.push(2); return [handle.count, handle.items] }"
                .to_string(),
        );
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use facade as facade\npub fn next() { return facade.via(facade) }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, [1, 2]]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[2, [1, 2, 2]]"
        );
    }

    #[test]
    fn origin_module_handles_find_values_moved_into_an_active_importer() {
        let mut modules = BTreeMap::new();
        modules.insert("leaf".to_string(), "let count = 0".to_string());
        modules.insert("facade".to_string(), "use leaf".to_string());
        let mut host = ModuleHost { modules };
        let mut instance = BopInstance::load(
            "use leaf as leaf\nuse facade\npub fn next() { count += 1; return [count, leaf.count] }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[1, 1]"
        );
        assert_eq!(
            instance.call("next", &[], &mut host).unwrap().inspect(),
            "[2, 2]"
        );
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

    struct ExternalValueHost {
        value: Option<Value>,
    }

    impl BopHost for ExternalValueHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            (name == "take_external").then(|| Ok(self.value.take().unwrap_or(Value::None)))
        }
    }

    #[test]
    fn external_host_values_are_free_until_the_instance_first_mutates_them() {
        let external = {
            let _suspended = ActiveMemoryGuard::__suspend();
            Value::new_array((0..256).map(Value::Int).collect())
        };
        let limits = BopLimits { max_steps: 100, max_memory: 64 };
        let mut host = ExternalValueHost { value: Some(external) };
        let mut instance = BopInstance::load(
            "let stored = none\npub fn keep() { stored = take_external() }\npub fn mutate() { stored.push(256) }\npub fn harmless() { return 1 }",
            &mut host,
            &limits,
        )
        .unwrap();

        instance.call("keep", &[], &mut host).unwrap();
        assert_eq!(instance.memory.used(), 0);

        let mutation_error = instance.call("mutate", &[], &mut host).unwrap_err();
        assert!(mutation_error.is_fatal);
        assert!(mutation_error.message.contains("Memory limit"));
        assert!(instance.memory.used() > limits.max_memory);

        let poisoned_error = instance.call("harmless", &[], &mut host).unwrap_err();
        assert!(poisoned_error.is_fatal);
        assert!(poisoned_error.message.contains("Memory limit"));
    }

    #[test]
    fn returned_values_keep_their_instance_receipt_until_the_last_owner_drops() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "pub fn make() { return [1, 2, 3, 4] }\npub fn harmless() { return none }",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(instance.memory.used(), 0);

        let retained = instance.call("make", &[], &mut host).unwrap();
        let retained_bytes = instance.memory.used();
        assert!(retained_bytes > 0);
        instance.call("harmless", &[], &mut host).unwrap();
        assert_eq!(instance.memory.used(), retained_bytes);

        let second_owner = retained.clone();
        drop(retained);
        assert_eq!(instance.memory.used(), retained_bytes);
        drop(second_owner);
        assert_eq!(instance.memory.used(), 0);
    }

    #[test]
    fn interleaved_instances_keep_independent_memory_accounts() {
        let mut host = Host;
        let source = "pub fn make(x) { return [x, x] }";
        let limits = BopLimits::standard();
        let mut first = BopInstance::load(source, &mut host, &limits).unwrap();
        let mut second = BopInstance::load(source, &mut host, &limits).unwrap();

        let first_value = first.call("make", &[Value::Int(1)], &mut host).unwrap();
        let first_bytes = first.memory.used();
        assert!(first_bytes > 0);
        assert_eq!(second.memory.used(), 0);

        let second_value = second.call("make", &[Value::Int(2)], &mut host).unwrap();
        let second_bytes = second.memory.used();
        assert!(second_bytes > 0);
        assert_eq!(first.memory.used(), first_bytes);

        drop(first_value);
        assert_eq!(first.memory.used(), 0);
        assert_eq!(second.memory.used(), second_bytes);
        drop(second_value);
        assert_eq!(second.memory.used(), 0);
    }

    struct HookAllocatingHost {
        retained: RefCell<Vec<Value>>,
    }

    impl HookAllocatingHost {
        fn retain_large(&self) {
            self.retained
                .borrow_mut()
                .push(Value::new_str("x".repeat(16 * 1024)));
        }
    }

    impl BopHost for HookAllocatingHost {
        fn call(
            &mut self,
            name: &str,
            _args: &[Value],
            _line: u32,
        ) -> Option<Result<Value, BopError>> {
            self.retain_large();
            (name == "host_value").then_some(Ok(Value::None))
        }

        fn on_print(&mut self, _message: &str) {
            self.retain_large();
        }

        fn function_hint(&self) -> &str {
            self.retain_large();
            "host hint"
        }

        fn on_tick(&mut self) -> Result<(), BopError> {
            self.retain_large();
            Ok(())
        }

        fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
            self.retain_large();
            (name == "hook").then(|| Ok(String::new()))
        }
    }

    #[test]
    fn every_host_hook_runs_with_instance_accounting_suspended() {
        let limits = BopLimits { max_steps: 100, max_memory: 64 };
        let mut host = HookAllocatingHost { retained: RefCell::new(Vec::new()) };
        let mut instance = BopInstance::load(
            "use hook\npub fn print_it() { print(\"ok\") }\npub fn host_it() { host_value() }\npub fn hint_it() { missing() }",
            &mut host,
            &limits,
        )
        .unwrap();
        assert_eq!(instance.memory.used(), 0);

        instance.call("print_it", &[], &mut host).unwrap();
        instance.call("host_it", &[], &mut host).unwrap();
        let error = instance.call("hint_it", &[], &mut host).unwrap_err();
        assert!(!error.is_fatal);
        assert!(error
            .friendly_hint
            .as_deref()
            .is_some_and(|hint| hint.contains("host hint")));
        assert_eq!(instance.memory.used(), 0);
        assert!(host.retained.borrow().len() >= 8);
    }

    #[test]
    fn reentry_rejection_precedes_target_and_arity_preflight() {
        let mut host = Host;
        let mut instance = BopInstance::load(
            "pub fn entry() {}",
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        instance.in_operation.set(true);
        let error = instance
            .call("missing", &[Value::None], &mut host)
            .unwrap_err();
        instance.in_operation.set(false);
        assert_eq!(error.line, Some(0));
        assert!(error.message.contains("cannot be re-entered"));
    }
}
