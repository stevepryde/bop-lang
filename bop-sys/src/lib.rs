//! Standard host integration for Bop.
//!
//! `bop-lang` contains the pure language implementation. This crate provides
//! the default host behavior for applications that want normal OS-backed
//! integration.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_host_does_not_handle_custom_calls_by_default() {
        let mut host = StandardHost::new();

        assert!(host.call("unknown", &[], 1).is_none());
    }
}

impl BopHost for StandardHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, BopError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        #[cfg(feature = "stdio")]
        println!("{}", message);

        #[cfg(not(feature = "stdio"))]
        let _ = message;
    }
}
