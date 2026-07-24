use bop::{BopError, BopHost, BopLimits, Value};

#[derive(Default)]
struct Host {
    output: Vec<String>,
}

impl BopHost for Host {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, BopError>> {
        match (name, args) {
            ("double", [Value::Int(value)]) => Some(
                value
                    .checked_mul(2)
                    .map(Value::Int)
                    .ok_or_else(|| BopError::runtime("double(value) overflowed", line)),
            ),
            ("double", _) => Some(Err(BopError::runtime(
                "double(value) expects one Int",
                line,
            ))),
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.output.push(message.to_owned());
    }

    fn function_hint(&self) -> &str {
        "Host functions: double(value)"
    }
}

fn run_example() -> Result<(), BopError> {
    let source = "print(double(21))";
    let limits = BopLimits::standard();
    let mut host = Host::default();

    bop::run(source, &mut host, &limits)?;
    bop_vm::run(source, &mut host, &limits)?;

    assert_eq!(host.output, ["42", "42"]);
    Ok(())
}

fn main() {
    run_example().expect("custom-host example failed");
}

#[cfg(test)]
mod tests {
    #[test]
    fn custom_host_runs_on_both_engines() {
        super::run_example().unwrap();
    }

    #[test]
    fn host_arithmetic_reports_overflow_without_panicking() {
        let mut host = super::Host::default();
        let error = bop::run(
            "double(9223372036854775807)",
            &mut host,
            &bop::BopLimits::standard(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("overflowed"));
    }
}
