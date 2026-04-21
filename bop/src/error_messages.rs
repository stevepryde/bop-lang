//! Shared error-message format helpers.
//!
//! Walker, VM, and AOT each previously carried their own copy
//! of `format!("Variable `{}` not found", name)` and friends.
//! The strings are usually identical but the differential
//! tests compare errors across engines by `.message` text, so
//! any drift would surface as a spurious test failure — or
//! worse, in one engine and not another if a differential test
//! didn't happen to exercise the path.
//!
//! Collecting the common messages here means:
//!
//! - One edit when a message needs rewording.
//! - The function signature documents the intended arguments
//!   (you can't accidentally swap `type_name` and `field`).
//! - AOT-emitted code calls these same helpers, so the
//!   runtime-generated error text is byte-identical to the
//!   walker's without the AOT needing to paste format strings.
//!
//! Only messages with 2+ copies across engines live here. One-
//! off per-engine messages (e.g. `"VM: stack underflow"`) stay
//! with their engine — there's nothing to deduplicate.

#[cfg(feature = "no_std")]
use alloc::{format, string::String};

/// `Variable `<name>` not found`.
pub fn variable_not_found(name: &str) -> String {
    format!("Variable `{}` not found", name)
}

/// `Function `<name>` not found`.
pub fn function_not_found(name: &str) -> String {
    format!("Function `{}` not found", name)
}

/// `Struct `<type_name>` has no field `<field>``.
/// Used by the walker's `FieldAccess` path, the VM's
/// `ConstructStruct` / `FieldGet` / `FieldSet`, and the AOT's
/// runtime `__bop_field_get` helper.
pub fn struct_has_no_field(type_name: &str, field: &str) -> String {
    format!("Struct `{}` has no field `{}`", type_name, field)
}

/// `Variant `<type_name>::<variant>` has no field `<field>``.
/// For struct-shaped enum-variant field reads.
pub fn variant_has_no_field(type_name: &str, variant: &str, field: &str) -> String {
    format!(
        "Variant `{}::{}` has no field `{}`",
        type_name, variant, field
    )
}

/// `Struct `<type_name>` is not declared`.
pub fn struct_not_declared(type_name: &str) -> String {
    format!("Struct `{}` is not declared", type_name)
}

/// `Enum `<type_name>` is not declared`.
pub fn enum_not_declared(type_name: &str) -> String {
    format!("Enum `{}` is not declared", type_name)
}

/// `Enum `<type_name>` has no variant `<variant>``.
pub fn enum_has_no_variant(type_name: &str, variant: &str) -> String {
    format!("Enum `{}` has no variant `{}`", type_name, variant)
}

/// `Can't read field `<field>` on <kind>` — `kind` is the
/// pretty type name (`"array"`, `"int"`, etc.).
pub fn cant_read_field(field: &str, kind: &str) -> String {
    format!("Can't read field `{}` on {}", field, kind)
}

/// `Can't assign to field `<field>` on <kind>`.
pub fn cant_assign_field(field: &str, kind: &str) -> String {
    format!("Can't assign to field `{}` on {}", field, kind)
}

/// `Can't call a <kind>` — when a non-`Value::Fn` value lands
/// in a call position.
pub fn cant_call_a(kind: &str) -> String {
    format!("Can't call a {}", kind)
}

/// `Can't iterate over <kind>` — when `for x in …` gets a
/// value that isn't array-like or string-like.
pub fn cant_iterate_over(kind: &str) -> String {
    format!("Can't iterate over {}", kind)
}

/// `<kind> doesn't have a .<method>() method` — the terminal
/// error from the method dispatcher when nothing matches.
pub fn no_such_method(kind: &str, method: &str) -> String {
    format!("{} doesn't have a .{}() method", kind, method)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_not_found_format() {
        assert_eq!(variable_not_found("x"), "Variable `x` not found");
    }

    #[test]
    fn struct_has_no_field_matches_legacy_format() {
        // The legacy `format!` spelled this with backticks
        // around both `type_name` and `field`. Locking it down
        // here so a rewrite can't drift without updating this
        // test too.
        assert_eq!(
            struct_has_no_field("Point", "z"),
            "Struct `Point` has no field `z`"
        );
    }

    #[test]
    fn variant_has_no_field_uses_double_colon() {
        assert_eq!(
            variant_has_no_field("Shape", "Rect", "r"),
            "Variant `Shape::Rect` has no field `r`"
        );
    }

    #[test]
    fn cant_read_field_formats_without_backticks_around_kind() {
        // The kind (type name) is a user-facing noun like
        // "array" — no backticks around it, matching the
        // legacy walker phrasing.
        assert_eq!(
            cant_read_field("x", "array"),
            "Can't read field `x` on array"
        );
    }
}
