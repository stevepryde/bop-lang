use bop::{BopError, BopHost, Value};

/// Standard host for running Bop programs in a normal OS process.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardHost;

/// Short name for the standard host.
pub use StandardHost as StdHost;

impl StandardHost {
    pub fn new() -> Self {
        Self
    }
}

impl BopHost for StandardHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, BopError>> {
        match name {
            "readline" => Some(crate::stdio::readline(args, line)),
            "read_file" => Some(crate::fs::read_file(args, line)),
            "write_file" => Some(crate::fs::write_file(args, line)),
            "append_file" => Some(crate::fs::append_file(args, line)),
            "file_exists" => Some(crate::fs::file_exists(args, line)),
            "env" => Some(crate::env::env(args, line)),
            "unix_time" => Some(crate::time::unix_time(args, line)),
            "unix_time_ms" => Some(crate::time::unix_time_ms(args, line)),
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        println!("{}", message);
    }

    fn function_hint(&self) -> &str {
        enabled_function_hint()
    }
}

fn enabled_function_hint() -> &'static str {
    "Available bop-sys functions: readline(prompt?), read_file(path), write_file(path, contents), append_file(path, contents), file_exists(path), env(name), unix_time(), unix_time_ms()"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_host_does_not_handle_unknown_calls_by_default() {
        let mut host = StandardHost::new();

        assert!(host.call("unknown", &[], 1).is_none());
    }

    #[test]
    fn standard_host_exposes_function_hint() {
        let host = StandardHost::new();

        assert!(host.function_hint().contains("bop-sys"));
    }

    #[test]
    fn standard_host_reads_and_writes_files() {
        let mut host = StandardHost::new();
        let path = temp_path("bop_sys_file_test.txt");
        let path_value = Value::new_str(path.to_string_lossy().into_owned());

        host.call(
            "write_file",
            &[path_value.clone(), Value::new_str("hello".to_string())],
            1,
        )
        .expect("write_file should be handled")
        .expect("write_file should succeed");

        host.call(
            "append_file",
            &[path_value.clone(), Value::new_str(" world".to_string())],
            1,
        )
        .expect("append_file should be handled")
        .expect("append_file should succeed");

        let exists = host
            .call("file_exists", std::slice::from_ref(&path_value), 1)
            .expect("file_exists should be handled")
            .expect("file_exists should succeed");
        assert!(matches!(exists, Value::Bool(true)));

        let contents = host
            .call("read_file", std::slice::from_ref(&path_value), 1)
            .expect("read_file should be handled")
            .expect("read_file should succeed");
        assert_eq!(contents.to_string(), "hello world");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn standard_host_returns_none_for_missing_env_vars() {
        let mut host = StandardHost::new();
        let name = Value::new_str("BOP_SYS_ENV_VAR_THAT_SHOULD_NOT_EXIST".to_string());

        let value = host
            .call("env", &[name], 1)
            .expect("env should be handled")
            .expect("env should succeed");

        assert!(matches!(value, Value::None));
    }

    #[test]
    fn standard_host_returns_unix_time() {
        let mut host = StandardHost::new();

        let value = host
            .call("unix_time_ms", &[], 1)
            .expect("unix_time_ms should be handled")
            .expect("unix_time_ms should succeed");

        match value {
            Value::Number(n) => assert!(n > 0.0),
            other => panic!("expected number, got {}", other.type_name()),
        }
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("{}_{}", std::process::id(), name));
        path
    }
}
