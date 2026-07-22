use bop::value::BopFn;
use bop::{BopError, BopHost, BopLimits, Value};
use bop_vm::BopInstance;
use std::collections::BTreeMap;
use std::rc::Rc;

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
fn executed_final_public_declarations_define_the_abi() {
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
    assert_eq!(
        instance
            .call("first", &[Value::Int(9)], &mut host)
            .unwrap()
            .inspect(),
        "9"
    );
    assert!(instance.call("gone", &[], &mut host).is_err());
}

#[test]
fn top_level_return_retains_prior_root_state_and_skips_later_entries() {
    let mut host = Host;
    let mut instance = BopInstance::load(
        "let count = 4\npub fn next() { count += 1; return count }\nreturn\npub fn skipped() {}",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    assert_eq!(instance.entry_points().len(), 1);
    assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "5");
    assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "6");
    assert!(instance.call("skipped", &[], &mut host).is_err());
}

#[test]
fn callbacks_have_instance_affinity_and_preflight_is_line_zero() {
    let source = "pub fn make() { return fn(x) { return x } }";
    let mut host = Host;
    let mut first = BopInstance::load(source, &mut host, &BopLimits::standard()).unwrap();
    let callback = first.call("make", &[], &mut host).unwrap();
    let arity = first.call_value(&callback, &[], &mut host).unwrap_err();
    assert_eq!(arity.line, Some(0));
    assert_eq!(
        first
            .call_value(&callback, &[Value::Int(7)], &mut host)
            .unwrap()
            .inspect(),
        "7"
    );

    let mut second = BopInstance::load(source, &mut host, &BopLimits::standard()).unwrap();
    let affinity = second
        .call_value(&callback, &[Value::Int(7)], &mut host)
        .unwrap_err();
    assert_eq!(affinity.line, Some(0));
    assert!(affinity.message.contains("different Bop engine instance"));
}

#[test]
fn try_call_wraps_foreign_affinity_and_callback_arity_preflight_errors() {
    let mut host = Host;
    let mut first = BopInstance::load(
        "pub fn make() { return fn() { return 7 } }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    let foreign = first.call("make", &[], &mut host).unwrap();

    let mut second = BopInstance::load(
        "pub fn probe(f) { return try_call(f) }\npub fn make_arity() { return fn(x) { return x } }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    let affinity = second.call("probe", &[foreign], &mut host).unwrap();
    assert!(affinity.inspect().contains("Result::Err"));
    assert!(affinity.inspect().contains("different Bop engine instance"));

    let wrong_arity = second.call("make_arity", &[], &mut host).unwrap();
    let arity = second.call("probe", &[wrong_arity], &mut host).unwrap();
    assert!(arity.inspect().contains("Result::Err"));
    assert!(arity.inspect().contains("expects 1 argument"));
}

#[test]
fn uncaught_errors_unwind_transient_state_but_keep_prior_mutations() {
    let mut host = Host;
    let mut instance = BopInstance::load(
        "let count = 0\npub fn fail() { count += 1; let local = [1, 2]; panic(\"boom\") }\npub fn read() { return count }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    assert!(instance.call("fail", &[], &mut host).is_err());
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "1");
    assert!(instance.call("fail", &[], &mut host).is_err());
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "2");
}

#[test]
fn callback_preframe_errors_do_not_erase_live_root_state() {
    let mut host = Host;
    let mut instance = BopInstance::load(
        "let count = 0\npub fn bump() { count += 1; return count }\npub fn read() { return count }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    assert_eq!(instance.call("bump", &[], &mut host).unwrap().inspect(), "1");

    let opaque_body: Rc<dyn std::any::Any> = Rc::new(17_u8);
    let external = Value::Fn(
        BopFn::try_new_compiled(Vec::new(), Vec::new(), opaque_body, None, 0, 0)
            .unwrap(),
    );
    assert!(instance.call_value(&external, &[], &mut host).is_err());
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "1");
}

#[test]
fn step_budget_resets_for_each_call() {
    let limits = BopLimits { max_steps: 20, max_memory: 1024 * 1024 };
    let mut host = Host;
    let mut instance = BopInstance::load(
        "pub fn work() { let total = 0; repeat 5 { total += 1 }; return total }",
        &mut host,
        &limits,
    )
    .unwrap();
    assert_eq!(instance.call("work", &[], &mut host).unwrap().inspect(), "5");
    assert_eq!(instance.call("work", &[], &mut host).unwrap().inspect(), "5");
}

struct ModuleHost {
    modules: BTreeMap<String, String>,
}

impl BopHost for ModuleHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, BopError>> {
        None
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        self.modules.get(name).cloned().map(Ok)
    }
}

#[test]
fn imported_module_values_are_authoritative_across_calls_and_callbacks() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "state".to_string(),
        "let count = 0\nlet items = [1]\nfn next() { count += 1; items.push(count); return [count, items] }\nfn via(handle) { count += 1; return handle.count }\nfn make() { return fn() { count += 1; return count } }"
            .to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = BopInstance::load(
        "use state as state\npub fn next() { return state.next() }\npub fn via() { return state.via(state) }\npub fn make() { return state.make() }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "[1, [1, 1]]"
    );
    assert_eq!(
        instance.call("next", &[], &mut host).unwrap().inspect(),
        "[2, [1, 1, 2]]"
    );
    assert_eq!(instance.call("via", &[], &mut host).unwrap().inspect(), "3");
    let callback = instance.call("make", &[], &mut host).unwrap();
    assert_eq!(
        instance
            .call_value(&callback, &[], &mut host)
            .unwrap()
            .inspect(),
        "4"
    );
    assert_eq!(instance.call("via", &[], &mut host).unwrap().inspect(), "5");
}

#[test]
fn facade_reexports_share_authoritative_values_with_the_declaring_module() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "leaf".to_string(),
        "let count = 0\nlet items = [0]\nfn bump() { count += 1; items.push(count); return [count, items] }\nfn make() { return fn() { count += 1; return count } }"
            .to_string(),
    );
    modules.insert("facade".to_string(), "use leaf".to_string());
    let mut host = ModuleHost { modules };
    let mut instance = BopInstance::load(
        "use facade as api\nuse leaf as direct\npub fn bump() { return api.bump() }\npub fn read() { return [api.count, direct.count, api.items, direct.items] }\npub fn make() { return api.make() }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();

    assert_eq!(
        instance.call("bump", &[], &mut host).unwrap().inspect(),
        "[1, [0, 1]]"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[1, 1, [0, 1], [0, 1]]"
    );
    let callback = instance.call("make", &[], &mut host).unwrap();
    assert_eq!(
        instance.call_value(&callback, &[], &mut host).unwrap().inspect(),
        "2"
    );
    assert_eq!(
        instance.call("read", &[], &mut host).unwrap().inspect(),
        "[2, 2, [0, 1], [0, 1]]"
    );
}

#[test]
fn flat_imports_read_current_authoritative_values_at_root_and_in_functions() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "leaf".to_string(),
        "let count = 0\nfn bump() { count += 1; return count }".to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = BopInstance::load(
        "use leaf\nuse leaf as direct\npub fn next() { return direct.bump() }\npub fn read_root() { return count }\npub fn read_selective() { use leaf.{count}; return count }\npub fn read_glob() { use leaf; return count }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();

    assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "1");
    assert_eq!(instance.call("read_root", &[], &mut host).unwrap().inspect(), "1");
    assert_eq!(
        instance
            .call("read_selective", &[], &mut host)
            .unwrap()
            .inspect(),
        "1"
    );
    assert_eq!(instance.call("next", &[], &mut host).unwrap().inspect(), "2");
    assert_eq!(instance.call("read_glob", &[], &mut host).unwrap().inspect(), "2");
}

#[test]
fn failed_facade_load_restores_forwarded_values_and_remains_retryable() {
    let mut modules = BTreeMap::new();
    modules.insert("leaf".to_string(), "let count = 0".to_string());
    modules.insert(
        "bad".to_string(),
        "use leaf\ncount += 1\npanic(\"bad facade\")".to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = BopInstance::load(
        "use leaf as leaf\npub fn load_bad() { use bad }\npub fn read() { return leaf.count }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();

    assert!(instance.call("load_bad", &[], &mut host).is_err());
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "1");
    assert!(instance.call("load_bad", &[], &mut host).is_err());
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "2");
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
fn recursive_and_cross_module_calls_restore_authoritative_environments_in_order() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "a".to_string(),
        "let count = 0\nfn recurse(n) { count += 1; if n > 0 { return recurse(n - 1) }; return count }\nfn touch() { count += 1; return count }\nfn enter(other, handle) { count += 10; let inner = other.enter(handle); count += 100; return [inner, count] }\nfn read() { return count }"
            .to_string(),
    );
    modules.insert(
        "b".to_string(),
        "let count = 0\nfn enter(other) { count += 10; let inner = other.touch(); count += 100; return [inner, count] }\nfn read() { return count }"
            .to_string(),
    );
    let mut host = ModuleHost { modules };
    let mut instance = BopInstance::load(
        "use a as a\nuse b as b\npub fn cross() { return a.enter(b, a) }\npub fn recurse() { return a.recurse(2) }\npub fn read() { return [a.read(), b.read()] }",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();

    assert_eq!(
        instance.call("cross", &[], &mut host).unwrap().inspect(),
        "[[11, 110], 111]"
    );
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "[111, 110]");
    assert_eq!(instance.call("recurse", &[], &mut host).unwrap().inspect(), "114");
    assert_eq!(instance.call("read", &[], &mut host).unwrap().inspect(), "[114, 110]");
}
