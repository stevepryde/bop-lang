//! Standard host integration for Bop.
//!
//! `bop-lang` contains the pure language implementation. This crate provides
//! the default host behavior for applications that want normal OS-backed
//! integration.

mod args;
mod env;
mod error;
mod fs;
mod host;
mod stdio;
mod time;

pub use host::{StandardHost, StdHost};
