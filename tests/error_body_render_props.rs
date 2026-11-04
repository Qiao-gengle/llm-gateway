//! Property 42: 错误体渲染
//!
//! **Validates: Requirements 24.1, 24.2, 24.6, 24.7**

use axum::http::StatusCode;
use llm_gateway::http::errors::{ClientProtocol, ErrorResponse};
use proptest::prelude::*;
use serde_json::Value;

/// 独立于 `src/http/errors.rs` 内部实现，按 design.md「HTTP 状态码与错误体」
/// 映射表给出期望的 (OpenAI type, OpenAI code, Anthropic type)。
fn expected_mapping(status: u16) -> (&'static str, Option<&'static str>, &'static str) {
    match status {
        400 => ("invalid_request_error", None, "invalid_request_error"),
        401 => ("invalid_request_error", Some("invalid_api_key"), "authentication_error"),
        403 => ("permission_error", None, "permission_error"),
        404 => ("invalid_request_error", Some("model_not_found"), "not_found_error"),
        429 => ("rate_limit_error", Some("rate_limit_exceeded"), "rate_limit_error"),
        502 => ("api_error", None, "api_error"),
        503 => ("api_error", None, "overloaded_error"),
        400..=499 => ("invalid_request_error", None, "invalid_request_error"),
        _ => ("api_error", None, "api_error"),
    }
}

fn arb_protocol() -> impl Strategy<Value = ClientProtocol> {
    prop_oneof![Just(ClientProtocol::OpenAi), Just(ClientProtocol::Anthropic)]
}

/// 非空消息：任意非空字符串（允许包含特殊字符，但排除空字符串）。
fn arb_message() -> impl Strategy<Value = String> {
    "[^\\x00]{1,200}".prop_filter("message must be non-empty", |s| !s.is_empty())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p42_error_render(
        status_code in 400u16..=599u16,
        protocol in arb_protocol(),
        message in arb_message(),
    ) {
        let status = StatusCode::from_u16(status_code).unwrap();
        let (expected_oa_type, expected_oa_code, expected_an_type) = expected_mapping(status_code);

        let resp = ErrorResponse::render(protocol, status, &message);

        // 状态码与 Content-Type。
        prop_assert_eq!(resp.status(), status);
        prop_assert_eq!(
            resp.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()),
            Some("application/json".to_string())
        );

        let body = ErrorResponse::body(protocol, status, &message);

        match protocol {
            ClientProtocol::OpenAi => {
                let obj = body.as_object().expect("openai body must be an object");
                prop_assert_eq!(obj.len(), 1);
                let err = obj.get("error").and_then(Value::as_object).expect("error must be an object");
                prop_assert_eq!(err.len(), 3);
                prop_assert_eq!(err.get("message").and_then(Value::as_str), Some(message.as_str()));
                prop_assert_eq!(err.get("type").and_then(Value::as_str), Some(expected_oa_type));
                match expected_oa_code {
                    Some(code) => prop_assert_eq!(err.get("code").and_then(Value::as_str), Some(code)),
                    None => prop_assert!(err.get("code").map(Value::is_null).unwrap_or(false)),
                }
            }
            ClientProtocol::Anthropic => {
                let obj = body.as_object().expect("anthropic body must be an object");
                prop_assert_eq!(obj.len(), 2);
                prop_assert_eq!(obj.get("type").and_then(Value::as_str), Some("error"));
                let err = obj.get("error").and_then(Value::as_object).expect("error must be an object");
                prop_assert_eq!(err.len(), 2);
                prop_assert_eq!(err.get("type").and_then(Value::as_str), Some(expected_an_type));
                prop_assert_eq!(err.get("message").and_then(Value::as_str), Some(message.as_str()));
            }
        }
    }
}
