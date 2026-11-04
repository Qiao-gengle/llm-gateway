//! Feature: llm-gateway
//! Property 30: 空参数输出 "{}"
//! Validates: Requirements 18.6, 38.4

use llm_gateway::protocol::openai::response::stringify_arguments;
use proptest::prelude::*;
use serde_json::{Map, Value};

/// Arbitrary JSON scalar/collection value, bounded in depth, used to build
/// non-empty JSON objects for the round-trip side of the property.
fn arb_json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| Value::Number(n.into())),
        ".*".prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 16, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(".{1,8}", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// A non-empty JSON object: at least one key/value pair.
fn arb_nonempty_object() -> impl Strategy<Value = Value> {
    prop::collection::btree_map(".{1,8}", arb_json_value(), 1..6)
        .prop_map(|m| Value::Object(m.into_iter().collect::<Map<String, Value>>()))
}

proptest! {
    // Feature: llm-gateway, Property 30: 空参数输出 "{}"
    #[test]
    fn p30_empty_arguments_stringify_missing_null_and_empty_object(_unused in 0u8..1) {
        // 缺失（None 由上游解析逻辑转为 Value::Null 后传入 stringify_arguments，
        // 这里直接覆盖 Null 与空对象两种表示）与空串在解析阶段已被归一化为 Value::Null。
        prop_assert_eq!(stringify_arguments(&Value::Null), "{}");
        prop_assert_eq!(stringify_arguments(&Value::Object(Map::new())), "{}");
    }

    // Feature: llm-gateway, Property 30: 空参数输出 "{}"
    #[test]
    fn p30_non_empty_object_round_trips(value in arb_nonempty_object()) {
        let s = stringify_arguments(&value);
        prop_assert_ne!(s.as_str(), "{}");
        let parsed: Value = serde_json::from_str(&s).expect("stringify_arguments must produce valid JSON");
        prop_assert_eq!(parsed, value);
    }
}
