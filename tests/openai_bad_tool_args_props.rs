//! Feature: llm-gateway, Property 22: 非法工具参数拒绝
//!
//! **Validates: Requirements 16.6**
//!
//! For all OpenAI assistant `tool_calls` 中的一个工具调用，若其 `function.arguments`
//! 是非空且不能解析为 JSON 的字符串，或能解析但结果不是对象（数组、字符串、数字、
//! 布尔、null），`openai::parse_request` 返回 `Err`，错误信息包含该工具调用的 id。

use llm_gateway::http::errors::ValidationReason;
use llm_gateway::protocol::openai::request::parse_request;
use proptest::prelude::*;
use serde_json::json;

/// `raw` 能否解析为 JSON 对象（本属性的排除条件：这类取值由
/// `parse_tool_arguments` 正常接受，不在该属性的覆盖范围内）。
fn is_valid_object_json(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw).map(|v| v.is_object()).unwrap_or(false)
}

fn make_body(id: &str, name: &str, arguments: &str) -> Vec<u8> {
    let body = json!({
        "model": "m",
        "messages": [{
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            }]
        }]
    });
    serde_json::to_vec(&body).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p22_invalid_tool_arguments_rejected(
        id in "[a-zA-Z0-9_]{1,16}",
        name in "[a-zA-Z0-9_]{1,16}",
        arguments in any::<String>(),
    ) {
        // 空串（去除空白后）视为 `{}`，不在本属性覆盖范围内（需求 16 第 6 条的前提是“非空”）。
        prop_assume!(!arguments.trim().is_empty());
        // 仅覆盖「非法 JSON」或「合法 JSON 但解析结果不是对象」的情形。
        prop_assume!(!is_valid_object_json(&arguments));

        let body = make_body(&id, &name, &arguments);
        let err = parse_request(&body).expect_err("invalid tool arguments must be rejected");
        prop_assert_eq!(err.field, format!("tool_call:{id}"));
        prop_assert_eq!(
            err.reason,
            ValidationReason::Unsupported("arguments must be a JSON object")
        );
    }
}
