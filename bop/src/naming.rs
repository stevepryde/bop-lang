//! Identifier classification: the shared rules every engine and the
//! parser consult to sort a name into naming buckets.
//!
//! # Rules
//!
//! Strip any leading underscores to get the "core":
//!
//! | core                                              | kind           |
//! |---------------------------------------------------|----------------|
//! | empty                                             | `Wildcard`     |
//! | starts uppercase, **all** uppercase/digit/`_`     | `Constant`     |
//! | starts uppercase, has any lowercase               | `Type`         |
//! | starts lowercase or digit                         | `Value`        |
//!
//! So `FOO` and `HTTP2` classify as `Constant`, `Foo` and `HttpClient`
//! classify as `Type`, and `foo` / `_bar` / `_1` classify as `Value`.
//!
//! **Two important rules at declaration sites:**
//!
//! - `struct` / `enum` / enum variant accept **either** `Type` or
//!   `Constant` — the bare "starts with a capital letter" rule.
//!   Single-letter variants like `enum Dir { N, E, S, W }` are fine.
//! - `const` accepts **only** `Constant` — the name must be all
//!   uppercase. `const Pi = 3.14` is rejected.
//!
//! Leading underscores mark "private by convention" — they don't
//! change the classification. `_foo` is a `Value`, `_Foo` is a
//! `Type`, `_FOO` is a `Constant`. Glob imports skip names that
//! start with an underscore; explicit selective or aliased imports
//! still expose them.

#[cfg(feature = "no_std")]
use alloc::{format, string::String};

/// The shape bucket an identifier string belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentKind {
    /// Binding name: `let` / `fn` / param / field / method / alias
    /// / module path segment / `for-in` var / pattern binding.
    Value,
    /// Type name that *isn't* pure-uppercase — `Foo`, `Http`,
    /// `HttpClient`, `MyStruct`, `_Internal`. Accepted at
    /// struct/enum/variant sites.
    Type,
    /// Pure-uppercase name — either a `const` declaration or a
    /// valid type name that happens to be all caps (`FOO`, `X`,
    /// `HTTP`, `Dir::N`). Accepted at struct/enum/variant sites
    /// *and* at `const` sites. Assigned-to in a source expression
    /// means "reassigning a constant" and is refused at parse
    /// time.
    Constant,
    /// Pure-underscore identifier (`_`, `__`) — match wildcard
    /// and "explicitly discard" let (`let _ = foo()`).
    Wildcard,
}

/// Classify `name` into its shape bucket. See the module docs.
pub fn classify(name: &str) -> IdentKind {
    let core = name.trim_start_matches('_');
    if core.is_empty() {
        return IdentKind::Wildcard;
    }
    let first = core.as_bytes()[0];
    if first.is_ascii_uppercase() {
        if core.bytes().any(|b| b.is_ascii_lowercase()) {
            IdentKind::Type
        } else {
            IdentKind::Constant
        }
    } else {
        // Lowercase letter or digit.
        IdentKind::Value
    }
}

/// `true` if the identifier has a leading underscore and isn't a
/// pure-underscore wildcard — the "private by convention" marker.
/// Glob imports skip these so a module's internals stay private
/// unless the caller asks explicitly.
pub fn is_private(name: &str) -> bool {
    name.starts_with('_') && !name.trim_start_matches('_').is_empty()
}

/// Does this name's shape suit a binding site (`let`, `fn`,
/// param, field, alias, `for-in`, match binding)? Wildcards
/// count — `let _ = foo()` is legal.
pub fn is_value_name(name: &str) -> bool {
    matches!(classify(name), IdentKind::Value | IdentKind::Wildcard)
}

/// Does this name's shape suit a type site (`struct`, `enum`,
/// enum variant)? Both `Type` and `Constant` shapes pass — the
/// rule is "starts with an uppercase letter."
pub fn is_type_name(name: &str) -> bool {
    matches!(classify(name), IdentKind::Type | IdentKind::Constant)
}

/// Does this name's shape suit a `const` site? Only pure-
/// uppercase names pass — `const Foo = 1` is rejected.
pub fn is_constant_name(name: &str) -> bool {
    matches!(classify(name), IdentKind::Constant)
}

/// Human-readable label for error messages.
pub fn kind_label(kind: IdentKind) -> &'static str {
    match kind {
        IdentKind::Value => "value",
        IdentKind::Type => "type",
        IdentKind::Constant => "constant",
        IdentKind::Wildcard => "wildcard",
    }
}

/// Build a "did you mean?" hint for a mis-shaped identifier at a
/// specific kind of site. The suggestion is best-effort — users
/// can always pick a different name — but most of the time the
/// obvious case transform produces something usable.
pub fn hint_for(expected_kind: &str, actual: &str) -> String {
    match expected_kind {
        "value" => {
            let is_all_upper = actual.chars().all(|c| !c.is_ascii_lowercase());
            if is_all_upper && actual.chars().any(|c| c.is_ascii_alphabetic()) {
                return format!(
                    "names bound by `let` / `fn` / params start with a lowercase letter. \
                     Did you mean to declare a constant? (`const {} = ...`)",
                    actual
                );
            }
            format!(
                "names bound by `let` / `fn` / params start with a lowercase letter. \
                 Try `{}`?",
                lowercase_first(actual)
            )
        }
        "type" => {
            format!(
                "type names start with an uppercase letter. Try `{}`?",
                capitalize_first(actual)
            )
        }
        "constant" => {
            format!(
                "`const` names are SCREAMING_SNAKE_CASE (all uppercase). Try `{}`?",
                all_caps(actual)
            )
        }
        _ => String::new(),
    }
}

fn capitalize_first(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for c in s.chars() {
        if first {
            out.extend(c.to_uppercase());
            first = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn lowercase_first(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for c in s.chars() {
        if first {
            out.extend(c.to_lowercase());
            first = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn all_caps(s: &str) -> String {
    s.chars().flat_map(|c| c.to_uppercase()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_shapes() {
        for name in [
            "foo", "my_var", "camelCase", "doTheThing", "_foo", "_bar", "__baz", "x1", "_1",
        ] {
            assert_eq!(classify(name), IdentKind::Value, "{name} should be Value");
        }
    }

    #[test]
    fn type_shapes_are_pascal_case() {
        for name in [
            "Entity", "Result", "Ok", "Err", "HttpClient", "_Internal", "Foo", "BarBaz",
        ] {
            assert_eq!(classify(name), IdentKind::Type, "{name} should be Type");
        }
    }

    #[test]
    fn constant_shapes_are_all_caps() {
        for name in [
            "PI",
            "MAX_SIZE",
            "HTTP_PORT",
            "HTTP",
            "_DEBUG",
            "__RESERVED",
            "X",
            "X2",
            "X_Y",
            "N",
        ] {
            assert_eq!(
                classify(name),
                IdentKind::Constant,
                "{name} should be Constant"
            );
        }
    }

    #[test]
    fn wildcards() {
        assert_eq!(classify("_"), IdentKind::Wildcard);
        assert_eq!(classify("__"), IdentKind::Wildcard);
    }

    #[test]
    fn type_sites_accept_both_type_and_constant_shapes() {
        // The parser's `is_type_name` accepts both so `enum Dir { N, E, S, W }`
        // and short-acronym types like `HTTP` are legal even though
        // classify() separates them.
        assert!(is_type_name("Foo"));
        assert!(is_type_name("FOO"));
        assert!(is_type_name("N"));
        assert!(!is_type_name("foo"));
        assert!(!is_type_name("_"));
    }

    #[test]
    fn constant_sites_require_all_caps() {
        assert!(is_constant_name("PI"));
        assert!(is_constant_name("MAX"));
        assert!(!is_constant_name("Pi"));
        assert!(!is_constant_name("pi"));
    }

    #[test]
    fn privacy_marker() {
        assert!(is_private("_foo"));
        assert!(is_private("_Foo"));
        assert!(is_private("_FOO"));
        assert!(!is_private("foo"));
        assert!(!is_private("Foo"));
        assert!(!is_private("FOO"));
        assert!(!is_private("_")); // wildcard, not private
    }
}
