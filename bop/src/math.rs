//! Tiny `f64` math facade that works under both `std` and
//! `no_std + libm`. `core::f64` doesn't expose `sqrt` / `sin` /
//! `cos` / etc. — those methods live in `std::f64`'s
//! platform-dependent math library — so the no_std build has to
//! forward to the pure-Rust `libm` crate instead. The std build
//! stays dep-free and calls the native `f64` methods directly.
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
//! - `libm` (for `no_std`) — uses the `libm` crate. Add
//!   `features = ["libm"]` to your dependency declaration.
//!
//! Building with `default-features = false` and neither `libm`
//! nor `std` is a compile error: there's no floating-point math
//! library available on the target, and the bop engine can't
//! reach the language's math builtins without one.

#![allow(dead_code)]

#[cfg(all(not(feature = "std"), not(feature = "libm")))]
compile_error!(
    "bop-lang in no_std mode needs the `libm` feature for \
     floating-point math. Add `features = [\"libm\"]` to your \
     bop-lang dependency, or re-enable the default `std` feature."
);

/// `√x`.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.sqrt()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::sqrt(x)
    }
}

/// `sin(x)` (radians).
#[inline]
pub fn sin(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.sin()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::sin(x)
    }
}

/// `cos(x)` (radians).
#[inline]
pub fn cos(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.cos()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::cos(x)
    }
}

/// `tan(x)` (radians).
#[inline]
pub fn tan(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.tan()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::tan(x)
    }
}

/// `⌊x⌋` — largest integer ≤ x, as a float.
#[inline]
pub fn floor(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.floor()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::floor(x)
    }
}

/// `⌈x⌉` — smallest integer ≥ x, as a float.
#[inline]
pub fn ceil(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.ceil()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::ceil(x)
    }
}

/// Round half-away-from-zero (matches `f64::round`).
#[inline]
pub fn round(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.round()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::round(x)
    }
}

/// Truncate toward zero — drop the fractional part.
#[inline]
pub fn trunc(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.trunc()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::trunc(x)
    }
}

/// `base ** exp` for floats.
#[inline]
pub fn powf(base: f64, exp: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        base.powf(exp)
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::pow(base, exp)
    }
}

/// Natural log.
#[inline]
pub fn ln(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.ln()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::log(x)
    }
}

/// `e^x`.
#[inline]
pub fn exp(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.exp()
    }
    #[cfg(all(not(feature = "std"), feature = "libm"))]
    {
        libm::exp(x)
    }
}
