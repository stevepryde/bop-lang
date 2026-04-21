//! Tiny `f64` math facade that works on both the default (std)
//! build and the `no_std` opt-in. `core::f64` doesn't expose
//! `sqrt` / `sin` / `cos` / etc. — those methods live in
//! `std::f64`'s platform-dependent math library — so the no_std
//! build has to forward to the pure-Rust `libm` crate instead.
//! The std build stays dep-free and calls the native `f64`
//! methods directly.
//!
//! Every bop math builtin and `ops::div` / `ops::rem`'s float
//! path goes through here rather than calling `x.sqrt()` /
//! `libm::sqrt(x)` at the point of use, so the rest of the
//! codebase stays `#[cfg]`-free.
//!
//! ## Feature
//!
//! The only math-related feature is `no_std`. Leave it off (the
//! default) to get `std::f64`'s native math with no external
//! deps. Enable it — with `default-features = false, features =
//! ["no_std"]` — when targeting bare-metal / embedded / edge
//! wasm. `no_std` pulls in the tiny pure-Rust `libm` crate.

#![allow(dead_code)]

/// `√x`.
#[inline]
pub fn sqrt(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.sqrt()
    }
    #[cfg(feature = "no_std")]
    {
        libm::sqrt(x)
    }
}

/// `sin(x)` (radians).
#[inline]
pub fn sin(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.sin()
    }
    #[cfg(feature = "no_std")]
    {
        libm::sin(x)
    }
}

/// `cos(x)` (radians).
#[inline]
pub fn cos(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.cos()
    }
    #[cfg(feature = "no_std")]
    {
        libm::cos(x)
    }
}

/// `tan(x)` (radians).
#[inline]
pub fn tan(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.tan()
    }
    #[cfg(feature = "no_std")]
    {
        libm::tan(x)
    }
}

/// `⌊x⌋` — largest integer ≤ x, as a float.
#[inline]
pub fn floor(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.floor()
    }
    #[cfg(feature = "no_std")]
    {
        libm::floor(x)
    }
}

/// `⌈x⌉` — smallest integer ≥ x, as a float.
#[inline]
pub fn ceil(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.ceil()
    }
    #[cfg(feature = "no_std")]
    {
        libm::ceil(x)
    }
}

/// Round half-away-from-zero (matches `f64::round`).
#[inline]
pub fn round(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.round()
    }
    #[cfg(feature = "no_std")]
    {
        libm::round(x)
    }
}

/// Truncate toward zero — drop the fractional part.
#[inline]
pub fn trunc(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.trunc()
    }
    #[cfg(feature = "no_std")]
    {
        libm::trunc(x)
    }
}

/// `base ** exp` for floats.
#[inline]
pub fn powf(base: f64, exp: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        base.powf(exp)
    }
    #[cfg(feature = "no_std")]
    {
        libm::pow(base, exp)
    }
}

/// Natural log.
#[inline]
pub fn ln(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.ln()
    }
    #[cfg(feature = "no_std")]
    {
        libm::log(x)
    }
}

/// `e^x`.
#[inline]
pub fn exp(x: f64) -> f64 {
    #[cfg(not(feature = "no_std"))]
    {
        x.exp()
    }
    #[cfg(feature = "no_std")]
    {
        libm::exp(x)
    }
}
