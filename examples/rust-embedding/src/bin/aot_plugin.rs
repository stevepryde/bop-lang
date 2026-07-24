use bop::{BopError, BopHost, BopLimits, Value};

include!(concat!(env!("OUT_DIR"), "/plugin.rs"));

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

fn run_example() -> Result<(), BopError> {
    let mut host = Host;
    let mut instance = plugin::BopInstance::load(&mut host, &BopLimits::standard())?;

    assert_eq!(
        instance
            .call("next", &[], &mut host)?
            .to_rust::<i64>()
            .unwrap(),
        1,
    );
    assert_eq!(
        instance
            .call("next", &[], &mut host)?
            .to_rust::<i64>()
            .unwrap(),
        2,
    );
    Ok(())
}

fn main() {
    run_example().expect("AOT plugin example failed");
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_plugin_compiles_and_persists_state() {
        super::run_example().unwrap();
    }
}
