//! Feature: llm-gateway, Property 43: 错误体脱敏
//! **Validates: Requirements 24.8, 13.7**

use axum::http::StatusCode;
use llm_gateway::http::errors::{ClientProtocol, ErrorResponse, GatewayError, RateLimitError, scrub};
use proptest::prelude::*;
use secrecy::SecretString;

/// 生成一个“看起来随机”的密钥串：前缀 + 若干字母数字字符，足够独特，
/// 不会与错误体中的固定文案（如 "model not found"）偶然重合。
fn secret_strategy(prefix: &'static str) -> impl Strategy<Value = String> {
    "[A-Za-z0-9]{16,40}".prop_map(move |s| format!("{prefix}{s}"))
}

/// 把 `secret` 任意次数地拼接进一段随机的周边文本中（可能出现 0 次，即完全不含密钥）。
fn embed_strategy(secret: String) -> impl Strategy<Value = String> {
    prop::collection::vec("[ -~]{0,12}".prop_map(|s| s), 0..4).prop_map(move |chunks| {
        let mut s = String::new();
        for (i, c) in chunks.iter().enumerate() {
            s.push_str(c);
            if i % 2 == 0 {
                s.push_str(&secret);
            }
        }
        s
    })
}

/// Upstream_Key 与一段可能多次嵌入该密钥的上游错误消息。
fn upstream_key_and_message_strategy() -> impl Strategy<Value = (String, String)> {
    secret_strategy("sk-up-").prop_flat_map(|key| {
        let message = embed_strategy(key.clone());
        message.prop_map(move |m| (key.clone(), m))
    })
}

/// 除 `UpstreamRejected` 之外的 `GatewayError` 变体：消息均为固定文案，
/// 不会包含任何随机生成的密钥。
fn fixed_error_strategy() -> impl Strategy<Value = GatewayError> {
    prop_oneof![
        Just(GatewayError::Unauthorized),
        Just(GatewayError::Forbidden),
        Just(GatewayError::ModelNotFound),
        Just(GatewayError::LimiterUnavailable),
        Just(GatewayError::AuthBackendUnavailable),
        Just(GatewayError::AllBreakersOpen),
        Just(GatewayError::Internal),
        any::<u32>().prop_map(|s| GatewayError::RateLimited(RateLimitError::Rpm { retry_after: s % 60 + 1 })),
        any::<u32>().prop_map(|s| GatewayError::RateLimited(RateLimitError::Tpm { retry_after: s % 60 + 1 })),
    ]
}

fn status_strategy() -> impl Strategy<Value = StatusCode> {
    (400u16..=599u16).prop_map(|c| StatusCode::from_u16(c).unwrap())
}

fn protocol_strategy() -> impl Strategy<Value = ClientProtocol> {
    prop_oneof![Just(ClientProtocol::OpenAi), Just(ClientProtocol::Anthropic)]
}

/// 渲染后的响应体（两种协议）里都不出现 `needle`。
fn assert_not_leaked(status: StatusCode, message: &str, needle: &str, label: &str) {
    for protocol in [ClientProtocol::OpenAi, ClientProtocol::Anthropic] {
        let body = ErrorResponse::body(protocol, status, message);
        let text = body.to_string();
        assert!(!text.contains(needle), "{label} 泄露到 {protocol:?} 响应体: {text}");

        let resp = ErrorResponse::render(protocol, status, message);
        assert_eq!(resp.status(), status);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `UpstreamRejected` 的上游消息中嵌入 Upstream_Key：经 `scrub` 处理后，
    /// 渲染出的两种协议响应体都不包含该 Upstream_Key，也不包含其它三类无关密钥。
    #[test]
    fn p43_error_scrub(
        (upstream_key, raw_message) in upstream_key_and_message_strategy(),
        client_key in secret_strategy("sk-gw-"),
        master_key in secret_strategy("mk-"),
        admin_token in secret_strategy("adm-"),
        status in status_strategy(),
    ) {
        let secret = SecretString::from(upstream_key.clone());
        let scrubbed = scrub(&raw_message, &secret);

        prop_assert!(!scrubbed.contains(upstream_key.as_str()), "scrub 未清除全部 Upstream_Key 出现");

        let err = GatewayError::upstream_rejected(status, Some(scrubbed));
        let message = err.message();

        for (needle, label) in [
            (upstream_key.as_str(), "Upstream_Key"),
            (client_key.as_str(), "Client_Key"),
            (master_key.as_str(), "Master_Key"),
            (admin_token.as_str(), "Admin_Token"),
        ] {
            assert_not_leaked(err.status(), &message, needle, label);
        }
    }

    /// 其它 `GatewayError` 变体的消息是固定文案，天然不含任何随机密钥；
    /// 渲染后的响应体同样不应包含它们。
    #[test]
    fn p43_error_scrub_fixed_variants(
        err in fixed_error_strategy(),
        upstream_key in secret_strategy("sk-up-"),
        client_key in secret_strategy("sk-gw-"),
        master_key in secret_strategy("mk-"),
        admin_token in secret_strategy("adm-"),
        protocol in protocol_strategy(),
    ) {
        let status = err.status();
        let message = err.message();
        let body = ErrorResponse::body(protocol, status, &message);
        let text = body.to_string();
        for secret in [&upstream_key, &client_key, &master_key, &admin_token] {
            prop_assert!(!text.contains(secret.as_str()));
        }
    }
}
