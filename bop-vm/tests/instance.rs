use bop::{BopError, BopHost, BopLimits, Value};
use bop_vm::BopInstance;

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
