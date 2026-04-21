use std::path::PathBuf;

use bop::{BopError, BopHost, Value};

/// Standard host for running Bop programs in a normal OS process.
#[derive(Debug, Clone, Default)]
pub struct StandardHost {
    /// Root directory used to resolve `import` paths. When `None`
    /// the current working directory at resolve time is used.
    module_root: Option<PathBuf>,
}

/// Short name for the standard host.
pub use StandardHost as StdHost;

impl StandardHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the root directory for module resolution. An
    /// `import foo.bar` then maps to `<root>/foo/bar.bop`.
    pub fn with_module_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.module_root = Some(root.into());
        self
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

    /// Resolve `import foo.bar.baz`:
    ///
    /// 1. **Stdlib first** — when the `bop-std` feature is on
    ///    (default), `std.*` names hit [`bop::stdlib::resolve`],
    ///    which returns bundled Bop source. Stdlib modules
    ///    never touch the filesystem, so they work in every
    ///    host (including ones that set a `module_root` pointed
    ///    at a directory with no stdlib). When the feature is
    ///    off the step is skipped, so `std.*` falls through to
    ///    the filesystem like any other name.
    /// 2. **Filesystem fallback** — the rest map to
    ///    `<root>/foo/bar/baz.bop`. Missing files return
    ///    `None` so the runtime can raise a clean
    ///    *module not found* error; I/O errors (e.g.
    ///    permissions) are surfaced as `Some(Err(...))`.
    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        #[cfg(feature = "bop-std")]
        {
            if let Some(src) = bop::stdlib::resolve(name) {
                return Some(Ok(src.to_string()));
            }
        }
        if !is_valid_module_name(name) {
            return Some(Err(crate::error::io_error(
                &format!("Invalid module name `{}`", name),
                None,
            )));
        }
        let root = match self.module_root.clone() {
            Some(r) => r,
            None => match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    return Some(Err(crate::error::io_error(
                        &format!("couldn't read current directory: {}", e),
                        None,
                    )));
                }
            },
        };
        let mut path = root;
        for segment in name.split('.') {
            path.push(segment);
        }
        path.set_extension("bop");
        match std::fs::read_to_string(&path) {
            Ok(source) => Some(Ok(source)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => Some(Err(crate::error::io_error(
                &format!("couldn't read module `{}`: {}", name, err),
                None,
            ))),
        }
    }
}

/// Module-name validation: non-empty dot-separated segments of
/// `[A-Za-z0-9_]+`. Rejects leading/trailing/double dots and any
/// character that could escape the module root (`/`, `..`, NUL,
/// drive letters, …).
fn is_valid_module_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.split('.').all(|seg| {
        !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
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

    #[test]
    fn resolve_module_maps_dotted_path_to_file() {
        let dir = std::env::temp_dir().join(format!(
            "bop_sys_resolve_{}",
            std::process::id()
        ));
        let sub = dir.join("math");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("util.bop");
        std::fs::write(&file, "let answer = 42").unwrap();

        let mut host = StandardHost::new().with_module_root(&dir);
        let source = host
            .resolve_module("math.util")
            .expect("module should resolve")
            .expect("should succeed");
        assert_eq!(source, "let answer = 42");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_module_missing_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "bop_sys_resolve_none_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut host = StandardHost::new().with_module_root(&dir);
        assert!(host.resolve_module("does_not_exist").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_module_rejects_bad_names() {
        let mut host = StandardHost::new();
        // `..` is forbidden — prevents path traversal.
        let result = host.resolve_module("..").expect("should error");
        assert!(result.is_err());
        let result = host.resolve_module("foo/bar").expect("should error");
        assert!(result.is_err());
    }
}
