use std::collections::BTreeMap;

use bop::value::{BUILTIN_MODULE_PATH, EnumPayload, MAX_VALUE_DEPTH, VALUE_DEPTH_ERROR_MESSAGE};
use bop::{BopError, BopHost, BopLimits, FromValue, IntoValue, Value, ValuePathSegment, bop_value};

#[test]
fn scalar_from_conversions_are_exact_and_lossless() {
    assert!(matches!(Value::from(()), Value::None));
    assert!(matches!(Value::from(i64::MIN), Value::Int(i64::MIN)));
    assert!(matches!(Value::from(i64::MAX), Value::Int(i64::MAX)));
    assert!(matches!(Value::from(u32::MAX), Value::Int(value) if value == i64::from(u32::MAX)));
    assert!(matches!(Value::from(1.25_f32), Value::Number(value) if value == 1.25));
    assert!(matches!(Value::from(true), Value::Bool(true)));
    assert!(matches!(Value::from("hello"), Value::Str(ref value) if value.as_str() == "hello"));
    assert!(matches!(Value::from(None::<i64>), Value::None));
    assert!(matches!(Value::from(Some(7_i32)), Value::Int(7)));
}

#[test]
fn wide_integers_are_checked_without_truncation() {
    assert!(matches!(
        (i64::MAX as u64).into_value().unwrap(),
        Value::Int(i64::MAX)
    ));

    let error = u64::MAX.into_value().unwrap_err();
    assert_eq!(error.expected(), "an integer in Bop's i64 range");
    assert!(error.actual().contains(&u64::MAX.to_string()));
    assert!(error.path().is_empty());

    let error = Value::Int(-1).to_rust::<u64>().unwrap_err();
    assert_eq!(error.expected(), "int in Rust `u64` range");
    assert_eq!(error.actual(), "integer -1");
}

#[test]
fn reverse_numeric_conversion_preserves_int_number_distinction() {
    assert_eq!(Value::Int(42).to_rust::<i64>().unwrap(), 42);
    assert_eq!(Value::Number(42.5).to_rust::<f64>().unwrap(), 42.5);
    assert_eq!(
        Value::Number(42.0).to_rust::<i64>().unwrap_err().actual(),
        "number"
    );
    assert_eq!(Value::Int(42).to_rust::<f64>().unwrap_err().actual(), "int");
}

#[test]
fn macro_builds_nested_values_and_is_hygienic() {
    mod shadowed_names {
        pub struct Value;
        pub trait IntoValue {}
    }

    let _ = core::mem::size_of::<shadowed_names::Value>();
    fn assert_shadow_trait<T: shadowed_names::IntoValue>() {}
    struct Shadow;
    impl shadowed_names::IntoValue for Shadow {}
    assert_shadow_trait::<Shadow>();

    assert!(matches!(bop_value!([]).unwrap(), Value::Array(ref values) if values.is_empty()));
    assert!(matches!(bop_value!({}).unwrap(), Value::Dict(ref entries) if entries.is_empty()));

    let value = bop_value!({
        "name": "Ada",
        "stats": { "hp": 100, "mp": 40 },
        "tags": ["engineer", none, "mathematician"],
    })
    .unwrap();

    let fields: BTreeMap<String, Value> = value.to_rust().unwrap();
    assert_eq!(fields["name"].to_rust::<&str>().unwrap(), "Ada");
    let stats: BTreeMap<String, i64> = fields["stats"].to_rust().unwrap();
    assert_eq!(
        stats,
        BTreeMap::from([("hp".into(), 100), ("mp".into(), 40)])
    );
    let tags: Vec<Option<String>> = fields["tags"].to_rust().unwrap();
    assert_eq!(
        tags,
        vec![Some("engineer".into()), None, Some("mathematician".into())]
    );
}

#[test]
fn nested_failures_report_a_structured_root_to_leaf_path() {
    let value = bop_value!([{ "stats": { "hp": "oops" } }]).unwrap();
    let error = value
        .to_rust::<Vec<BTreeMap<String, BTreeMap<String, i64>>>>()
        .unwrap_err();

    assert_eq!(error.expected(), "int");
    assert_eq!(error.actual(), "string");
    assert_eq!(
        error.path(),
        &[
            ValuePathSegment::Index(0),
            ValuePathSegment::Key("stats".into()),
            ValuePathSegment::Key("hp".into()),
        ]
    );
    assert_eq!(
        error.to_string(),
        "value conversion failed at $[0][\"stats\"][\"hp\"]: expected int, got string"
    );
}

#[test]
fn btree_maps_are_deterministic_and_duplicate_bop_keys_are_rejected() {
    let map = BTreeMap::from([("z", 1_i64), ("a", 2_i64)]);
    let value = map.into_value().unwrap();
    let Value::Dict(entries) = &value else {
        panic!("expected dict");
    };
    assert_eq!(
        entries
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );

    let duplicate = Value::try_new_dict(
        vec![
            ("same".into(), Value::Int(1)),
            ("same".into(), Value::Int(2)),
        ],
        0,
    )
    .unwrap();
    let error = duplicate.to_rust::<BTreeMap<String, i64>>().unwrap_err();
    assert_eq!(error.path(), &[ValuePathSegment::Key("same".into())]);
    assert!(error.actual().contains("duplicate key"));
}

#[test]
fn btree_map_forward_failures_add_each_key_to_the_path_once() {
    let map = BTreeMap::from([("hp", u64::MAX)]);
    let error = map.into_value().unwrap_err();

    assert_eq!(error.path(), &[ValuePathSegment::Key("hp".into())]);
    assert_eq!(
        error.to_string(),
        format!(
            "value conversion failed at $[\"hp\"]: expected an integer in Bop's i64 range, got integer {}",
            u64::MAX
        )
    );
}

#[test]
fn rust_results_round_trip_only_through_the_canonical_builtin_shape() {
    let value = Ok::<Vec<i64>, &str>(vec![1, 2, 3]).into_value().unwrap();
    let Value::EnumVariant(variant) = &value else {
        panic!("expected enum variant");
    };
    assert_eq!(variant.module_path(), BUILTIN_MODULE_PATH);
    assert_eq!(variant.type_name(), "Result");
    assert_eq!(variant.variant(), "Ok");
    assert!(matches!(variant.payload(), EnumPayload::Tuple(values) if values.len() == 1));
    assert_eq!(
        value.to_rust::<Result<Vec<i64>, &str>>().unwrap(),
        Ok(vec![1, 2, 3])
    );

    let error_value = Err::<i64, _>("borrowed").into_value().unwrap();
    assert_eq!(
        error_value.to_rust::<Result<i64, &str>>().unwrap(),
        Err("borrowed")
    );

    let user_result = Value::new_enum_tuple(
        "user.module".into(),
        "Result".into(),
        "Ok".into(),
        vec![Value::Int(1)],
    );
    assert!(user_result.to_rust::<Result<i64, String>>().is_err());

    let malformed = Value::new_enum_tuple(
        BUILTIN_MODULE_PATH.into(),
        "Result".into(),
        "Err".into(),
        Vec::new(),
    );
    let error = malformed.to_rust::<Result<i64, String>>().unwrap_err();
    assert_eq!(
        error.path(),
        &[ValuePathSegment::ResultVariant("Err".into())]
    );
}

#[test]
fn borrowed_extraction_does_not_copy_string_storage() {
    let value = Value::from("zero-copy");
    let Value::Str(storage) = &value else {
        panic!("expected string");
    };
    let borrowed: &str = FromValue::from_value(&value).unwrap();
    assert_eq!(borrowed.as_ptr(), storage.as_str().as_ptr());

    let cloned = value.to_rust::<Value>().unwrap();
    assert_eq!(cloned.to_rust::<&str>().unwrap(), "zero-copy");
}

#[test]
fn recursive_conversions_enforce_value_depth_without_panicking() {
    let mut value = Value::None;
    for _ in 0..MAX_VALUE_DEPTH {
        value = vec![value].into_value().unwrap();
    }

    let error = vec![value].into_value().unwrap_err();
    assert_eq!(error.expected(), "a Bop value within runtime limits");
    assert_eq!(error.actual(), VALUE_DEPTH_ERROR_MESSAGE);
}

#[test]
fn conversions_fit_naturally_at_the_bop_host_boundary() {
    #[derive(Default)]
    struct Host {
        output: Vec<String>,
    }

    impl BopHost for Host {
        fn call(
            &mut self,
            name: &str,
            args: &[Value],
            line: u32,
        ) -> Option<Result<Value, BopError>> {
            if name != "sum_values" {
                return None;
            }
            Some((|| {
                let input = args
                    .first()
                    .ok_or_else(|| BopError::runtime("sum_values expects an array", line))?;
                let values: Vec<i64> = input
                    .to_rust()
                    .map_err(|error| BopError::runtime(error.to_string(), line))?;
                values
                    .into_iter()
                    .sum::<i64>()
                    .into_value()
                    .map_err(|error| BopError::runtime(error.to_string(), line))
            })())
        }

        fn on_print(&mut self, message: &str) {
            self.output.push(message.into());
        }
    }

    let mut host = Host::default();
    bop::run(
        "print(sum_values([10, 20, 12]))",
        &mut host,
        &BopLimits::standard(),
    )
    .unwrap();
    assert_eq!(host.output, ["42"]);
}
