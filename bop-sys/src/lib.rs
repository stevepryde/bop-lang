//! Standard host integration for Bop.
//!
//! `bop-lang` contains the pure language implementation. This crate provides
//! the default host behavior for applications that want normal OS-backed
//! integration.

use std::string::String;

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
            "readline" => Some(readline(args, line)),
            "read_file" => Some(read_file(args, line)),
            "write_file" => Some(write_file(args, line)),
            "append_file" => Some(append_file(args, line)),
            "file_exists" => Some(file_exists(args, line)),
            "env" => Some(env(args, line)),
            "unix_time" => Some(unix_time(args, line)),
            "unix_time_ms" => Some(unix_time_ms(args, line)),
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

fn readline(args: &[Value], line: u32) -> Result<Value, BopError> {
    use std::io::{self, Write};

    match args.len() {
        0 => {}
        1 => {
            print!("{}", expect_string("readline", &args[0], line)?);
            io::stdout()
                .flush()
                .map_err(|e| error(line, format!("readline failed to flush stdout: {}", e)))?;
        }
        _ => {
            return Err(error(
                line,
                format!(
                    "`readline` expects 0 or 1 arguments, but got {}",
                    args.len()
                ),
            ));
        }
    }

    let mut input = String::new();
    let bytes = io::stdin()
        .read_line(&mut input)
        .map_err(|e| error(line, format!("readline failed: {}", e)))?;

    if bytes == 0 {
        return Ok(Value::None);
    }

    trim_line_ending(&mut input);
    Ok(Value::new_str(input))
}

fn read_file(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("read_file", args, 1, line)?;
    let path = expect_string("read_file", &args[0], line)?;

    std::fs::read_to_string(path)
        .map(Value::new_str)
        .map_err(|e| error(line, format!("read_file failed for `{}`: {}", path, e)))
}

fn write_file(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("write_file", args, 2, line)?;
    let path = expect_string("write_file", &args[0], line)?;
    let contents = expect_string("write_file", &args[1], line)?;

    std::fs::write(path, contents)
        .map(|_| Value::None)
        .map_err(|e| error(line, format!("write_file failed for `{}`: {}", path, e)))
}

fn append_file(args: &[Value], line: u32) -> Result<Value, BopError> {
    use std::io::Write;

    expect_args("append_file", args, 2, line)?;
    let path = expect_string("append_file", &args[0], line)?;
    let contents = expect_string("append_file", &args[1], line)?;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| error(line, format!("append_file failed for `{}`: {}", path, e)))?;

    file.write_all(contents.as_bytes())
        .map(|_| Value::None)
        .map_err(|e| error(line, format!("append_file failed for `{}`: {}", path, e)))
}

fn file_exists(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("file_exists", args, 1, line)?;
    let path = expect_string("file_exists", &args[0], line)?;

    Ok(Value::Bool(std::path::Path::new(path).exists()))
}

fn env(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("env", args, 1, line)?;
    let name = expect_string("env", &args[0], line)?;

    match std::env::var(name) {
        Ok(value) => Ok(Value::new_str(value)),
        Err(std::env::VarError::NotPresent) => Ok(Value::None),
        Err(std::env::VarError::NotUnicode(_)) => Err(error(
            line,
            format!("env variable `{}` is not valid Unicode", name),
        )),
    }
}

fn unix_time(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("unix_time", args, 0, line)?;

    Ok(Value::Number(unix_duration(line)?.as_secs_f64()))
}

fn unix_time_ms(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("unix_time_ms", args, 0, line)?;

    Ok(Value::Number(unix_duration(line)?.as_millis() as f64))
}

fn unix_duration(line: u32) -> Result<std::time::Duration, BopError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| error(line, format!("system clock is before Unix epoch: {}", e)))
}

fn enabled_function_hint() -> &'static str {
    "Available bop-sys functions: readline(prompt?), read_file(path), write_file(path, contents), append_file(path, contents), file_exists(path), env(name), unix_time(), unix_time_ms()"
}

fn expect_args(name: &str, args: &[Value], expected: usize, line: u32) -> Result<(), BopError> {
    if args.len() == expected {
        return Ok(());
    }

    Err(error(
        line,
        format!(
            "`{}` expects {} argument{}, but got {}",
            name,
            expected,
            if expected == 1 { "" } else { "s" },
            args.len()
        ),
    ))
}

fn expect_string<'a>(name: &str, value: &'a Value, line: u32) -> Result<&'a str, BopError> {
    match value {
        Value::Str(s) => Ok(s.as_str()),
        other => Err(error(
            line,
            format!("`{}` expects a string, but got {}", name, other.type_name()),
        )),
    }
}

fn trim_line_ending(input: &mut String) {
    if input.ends_with('\n') {
        input.pop();
        if input.ends_with('\r') {
            input.pop();
        }
    }
}

fn error(line: u32, message: impl Into<String>) -> BopError {
    BopError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: None,
    }
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
