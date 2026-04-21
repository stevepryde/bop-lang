//! Tiny `f64` math facade that works under both `std` and
//! `no_std`. `core::f64` doesn't expose `sqrt` / `sin` / `cos` /
//! etc. — those methods live in `std::f64`'s platform-dependent
//! math library — so the no_std build has to forward to the
//! pure-Rust `libm` crate instead. The std build stays dep-free
//! and calls the native `f64` methods directly.
//!
//! Every bop math builtin and `ops::div` / `ops::rem`'s float
//! path goes through here rather than calling `x.sqrt()` /
//! `libm::sqrt(x)` at the point of use, so the rest of the
//! codebase stays `#[cfg]`-free.
//!
//! ## Feature requirements
//!
//! Enable exactly one of:
//!
//! - `std` (default) — uses `f64`'s built-in math methods. No
//!   external deps.
//! - `no_std` — for bare-metal / edge wasm / embedded targets.
//!   Pulls in the tiny pure-Rust `libm` crate internally.
//!   Enable with `default-features = false, features =
//!   ["no_std"]` in your `Cargo.toml`.
//!
//! Building with `default-features = false` and no `no_std`
//! feature is a compile error: there's no floating-point math
//! library available on the target, and the bop engine can't
//! reach the language's math builtins without one. The
//! `compile_error!` below is the *primary* diagnostic users
//! see; each fn also has a panicking fallback in the no-feature
//! cfg branch so the compiler doesn't cascade into a pile of
//! `expected f64, found ()` errors that would bury the real fix.

#![allow(dead_code)]

#[cfg(all(not(feature = "std"), not(feature = "no_std")))]
compile_error!(
    "bop-lang needs either the default `std` feature or the \
     `no_std` feature for floating-point math. Add \
     `features = [\"no_std\"]` to your bop-lang dependency, or \
     re-enable the default `std` feature."
);

// The "no feature enabled" fallback for every 1-arg math fn —
// keeps the function's return type `f64` under the broken cfg
// so the type-checker doesn't chase the error into caller
// sites. `compile_error!` above fires first at build time, so
// this is genuinely unreachable.
#[cfg(all(not(feature = "std"), not(feature = "no_std")))]
#[inline]
fn no_feature_f1(_: f64) -> f64 {
    unreachable!("enable `std` or `no_std` feature (see compile_error above)")
}

/// `√x`.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.sqrt()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::sqrt(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `sin(x)` (radians).
#[inline]
pub fn sin(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.sin()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::sin(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `cos(x)` (radians).
#[inline]
pub fn cos(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.cos()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::cos(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `tan(x)` (radians).
#[inline]
pub fn tan(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.tan()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::tan(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `⌊x⌋` — largest integer ≤ x, as a float.
#[inline]
pub fn floor(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.floor()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::floor(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `⌈x⌉` — smallest integer ≥ x, as a float.
#[inline]
pub fn ceil(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.ceil()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::ceil(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// Round half-away-from-zero (matches `f64::round`).
#[inline]
pub fn round(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.round()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::round(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// Truncate toward zero — drop the fractional part.
#[inline]
pub fn trunc(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.trunc()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::trunc(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `base ** exp` for floats.
#[inline]
pub fn powf(base: f64, exp: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        base.powf(exp)
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::pow(base, exp)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        let _ = exp;
        no_feature_f1(base)
    }
}

/// Natural log.
#[inline]
pub fn ln(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.ln()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::log(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}

/// `e^x`.
#[inline]
pub fn exp(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.exp()
    }
    #[cfg(all(not(feature = "std"), feature = "no_std"))]
    {
        libm::exp(x)
    }
    #[cfg(all(not(feature = "std"), not(feature = "no_std")))]
    {
        no_feature_f1(x)
    }
}
