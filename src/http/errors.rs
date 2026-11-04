//! 错误类型：请求阶段统一错误 `GatewayError`、校验/限流错误，以及 Admin_API 错误体。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::db::{DbError, conflict_status};

/// 客户端使用的协议，决定错误体与响应格式。
///
/// 对应 design.md「IR」一节的 `ClientProtocol`；任务 5.1 落地 `src/ir.rs` 后，
/// 本定义应替换为对 `crate::ir::ClientProtocol` 的引用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientProtocol {
    OpenAi,
    Anthropic,
}

/// 请求体校验失败（parse_request），`field` 用于错误信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub reason: ValidationReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationReason {
    InvalidJson,
    NotObject,
    Missing,
    InvalidType,
    Empty,
    Unsupported(&'static str),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match &self.reason {
            ValidationReason::InvalidJson => "is not valid JSON".to_string(),
            ValidationReason::NotObject => "must be a JSON object".to_string(),
            ValidationReason::Missing => "is required".to_string(),
            ValidationReason::InvalidType => "has an invalid type".to_string(),
            ValidationReason::Empty => "must not be empty".to_string(),
            ValidationReason::Unsupported(what) => format!("is unsupported: {what}"),
        };
        write!(f, "field '{}' {}", self.field, reason)
    }
}

impl std::error::Error for ValidationError {}

/// 限流拒绝；`retry_after` 单位为秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitError {
    Rpm { retry_after: u32 },
    Tpm { retry_after: u32 },
}

impl RateLimitError {
    pub fn retry_after(&self) -> u32 {
        match self {
            RateLimitError::Rpm { retry_after } | RateLimitError::Tpm { retry_after } => *retry_after,
        }
    }
}

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RateLimitError::Rpm { .. } => write!(f, "requests per minute limit exceeded"),
            RateLimitError::Tpm { .. } => write!(f, "tokens per minute limit exceeded"),
        }
    }
}

impl std::error::Error for RateLimitError {}

/// Failover 原因分类，对应指标标签 `reason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailoverReason {
    Connect,
    Timeout,
    Http429,
    Http5xx,
    InvalidResponse,
}

impl FailoverReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailoverReason::Connect => "connect",
            FailoverReason::Timeout => "timeout",
            FailoverReason::Http429 => "http_429",
            FailoverReason::Http5xx => "http_5xx",
            FailoverReason::InvalidResponse => "invalid_response",
        }
    }
}

/// 请求阶段统一错误，Bytes_Sent 之前使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    BadRequest(String),
    Unauthorized,
    Forbidden,
    ModelNotFound,
    RateLimited(RateLimitError),
    LimiterUnavailable,
    AuthBackendUnavailable,
    AllBreakersOpen,
    UpstreamExhausted { last_reason: FailoverReason },
    UpstreamRejected { status: StatusCode, message: String },
    Internal,
}

impl GatewayError {
    /// 对应的 HTTP 状态码（见 design.md「HTTP 状态码与错误体」）。
    pub fn status(&self) -> StatusCode {
        match self {
            GatewayError::BadRequest(_) => StatusCode::BAD_REQUEST,
            GatewayError::Unauthorized => StatusCode::UNAUTHORIZED,
            GatewayError::Forbidden => StatusCode::FORBIDDEN,
            GatewayError::ModelNotFound => StatusCode::NOT_FOUND,
            GatewayError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            GatewayError::UpstreamExhausted { .. } => StatusCode::BAD_GATEWAY,
            GatewayError::UpstreamRejected { status, .. } => *status,
            GatewayError::LimiterUnavailable
            | GatewayError::AuthBackendUnavailable
            | GatewayError::AllBreakersOpen => StatusCode::SERVICE_UNAVAILABLE,
            GatewayError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 响应体中使用的非空错误消息（见 design.md「HTTP 状态码与错误体」，需求 24 第 1 条）。
    pub fn message(&self) -> String {
        match self {
            GatewayError::BadRequest(m) => m.clone(),
            GatewayError::Unauthorized => "authentication failed".to_string(),
            GatewayError::Forbidden => "this api key does not have access to the requested model".to_string(),
            GatewayError::ModelNotFound => "model not found".to_string(),
            GatewayError::RateLimited(e) => e.to_string(),
            GatewayError::LimiterUnavailable => "rate limiter unavailable".to_string(),
            GatewayError::AuthBackendUnavailable => "authentication backend unavailable".to_string(),
            GatewayError::AllBreakersOpen => "all upstream providers are unavailable".to_string(),
            GatewayError::UpstreamExhausted { .. } => "all upstream attempts failed".to_string(),
            GatewayError::UpstreamRejected { message, .. } => message.clone(),
            GatewayError::Internal => "internal server error".to_string(),
        }
    }

    /// 构造 `UpstreamRejected`：`message` 为空（或无法从上游错误体解析）时退化为
    /// `upstream returned HTTP <status>`（需求 24 第 5 条）。
    pub fn upstream_rejected(status: StatusCode, message: Option<String>) -> Self {
        let message = match message {
            Some(m) if !m.trim().is_empty() => m,
            _ => format!("upstream returned HTTP {}", status.as_u16()),
        };
        GatewayError::UpstreamRejected { status, message }
    }
}

/// 把 `message` 中出现的 `secret` 明文替换为 `[REDACTED]`（需求 24 第 8 条、需求 13 第 7 条）。
///
/// 这是唯一会改动上游原始字符串的情况：错误响应里仅有的外部字符串就是上游
/// `error.message`，在放入响应前经过本函数清除 Upstream_Key。
pub fn scrub(message: &str, secret: &SecretString) -> String {
    let exposed = secret.expose_secret();
    if exposed.is_empty() {
        return message.to_string();
    }
    message.replace(exposed, "[REDACTED]")
}

/// 按 Client_Protocol 与状态码渲染错误响应体（design.md「HTTP 状态码与错误体」，需求 24 第 2、6、7 条）。
///
/// `render` 自身是纯函数（`body`），只在最外层包一层 `IntoResponse`。
pub struct ErrorResponse;

impl ErrorResponse {
    /// 纯函数：给定协议、状态码与非空消息，返回严格符合该协议格式的响应体。
    pub fn body(protocol: ClientProtocol, status: StatusCode, message: &str) -> Value {
        let (error_type, code) = Self::type_and_code(protocol, status);
        match protocol {
            ClientProtocol::OpenAi => {
                let code = match code {
                    Some(c) => Value::String(c.to_string()),
                    None => Value::Null,
                };
                json!({ "error": { "message": message, "type": error_type, "code": code } })
            }
            ClientProtocol::Anthropic => {
                json!({ "type": "error", "error": { "type": error_type, "message": message } })
            }
        }
    }

    /// 完整的 axum 响应：状态码为 `status`，`Content-Type: application/json`。
    pub fn render(protocol: ClientProtocol, status: StatusCode, message: &str) -> Response {
        (status, Json(Self::body(protocol, status, message))).into_response()
    }

    /// 状态码 → (`type`, `code`) 映射；`code` 只在 OpenAI 的部分状态码上取非 `null` 值。
    /// 表中未列出的其他 4xx 取 `invalid_request_error`，其他 5xx 取 `api_error`。
    fn type_and_code(protocol: ClientProtocol, status: StatusCode) -> (&'static str, Option<&'static str>) {
        let code = status.as_u16();
        match protocol {
            ClientProtocol::OpenAi => match code {
                400 => ("invalid_request_error", None),
                401 => ("invalid_request_error", Some("invalid_api_key")),
                403 => ("permission_error", None),
                404 => ("invalid_request_error", Some("model_not_found")),
                429 => ("rate_limit_error", Some("rate_limit_exceeded")),
                502 => ("api_error", None),
                503 => ("api_error", None),
                400..=499 => ("invalid_request_error", None),
                _ => ("api_error", None),
            },
            ClientProtocol::Anthropic => match code {
                400 => ("invalid_request_error", None),
                401 => ("authentication_error", None),
                403 => ("permission_error", None),
                404 => ("not_found_error", None),
                429 => ("rate_limit_error", None),
                502 => ("api_error", None),
                503 => ("overloaded_error", None),
                400..=499 => ("invalid_request_error", None),
                _ => ("api_error", None),
            },
        }
    }
}


impl From<ValidationError> for GatewayError {
    fn from(e: ValidationError) -> Self {
        GatewayError::BadRequest(e.to_string())
    }
}

impl From<RateLimitError> for GatewayError {
    fn from(e: RateLimitError) -> Self {
        GatewayError::RateLimited(e)
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::BadRequest(m) => write!(f, "bad request: {m}"),
            GatewayError::Unauthorized => write!(f, "unauthorized"),
            GatewayError::Forbidden => write!(f, "forbidden"),
            GatewayError::ModelNotFound => write!(f, "model not found"),
            GatewayError::RateLimited(e) => write!(f, "rate limited: {e}"),
            GatewayError::LimiterUnavailable => write!(f, "rate limiter unavailable"),
            GatewayError::AuthBackendUnavailable => write!(f, "auth backend unavailable"),
            GatewayError::AllBreakersOpen => write!(f, "all upstream providers are unavailable"),
            GatewayError::UpstreamExhausted { last_reason } => {
                write!(f, "upstream exhausted: {}", last_reason.as_str())
            }
            GatewayError::UpstreamRejected { status, .. } => write!(f, "upstream rejected: HTTP {status}"),
            GatewayError::Internal => write!(f, "internal error"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// Admin_API 错误，响应体 `{"error":{"type","message","field"?}}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminError {
    Unauthorized,
    Validation { message: String, field: Option<String> },
    NotFound { message: String },
    Conflict { message: String },
    Unavailable,
}

impl AdminError {
    pub fn validation(field: impl Into<String>, message: impl Into<String>) -> Self {
        AdminError::Validation { message: message.into(), field: Some(field.into()) }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AdminError::Unauthorized => StatusCode::UNAUTHORIZED,
            AdminError::Validation { .. } => StatusCode::BAD_REQUEST,
            AdminError::NotFound { .. } => StatusCode::NOT_FOUND,
            AdminError::Conflict { .. } => StatusCode::CONFLICT,
            AdminError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self {
            AdminError::Unauthorized => "unauthorized",
            AdminError::Validation { .. } => "invalid_request",
            AdminError::NotFound { .. } => "not_found",
            AdminError::Conflict { .. } => "conflict",
            AdminError::Unavailable => "unavailable",
        }
    }

    fn message(&self) -> String {
        match self {
            AdminError::Unauthorized => "admin authentication failed".to_string(),
            AdminError::Validation { message, .. }
            | AdminError::NotFound { message }
            | AdminError::Conflict { message } => message.clone(),
            AdminError::Unavailable => "database unavailable".to_string(),
        }
    }

    pub fn body(&self) -> Value {
        let mut err = json!({ "type": self.error_type(), "message": self.message() });
        if let AdminError::Validation { field: Some(f), .. } = self {
            err["field"] = Value::String(f.clone());
        }
        json!({ "error": err })
    }
}

impl From<DbError> for AdminError {
    /// 23505 / 23503 → 409；其他数据库错误 → 503。
    fn from(e: DbError) -> Self {
        match e.sqlstate().and_then(conflict_status) {
            Some(_) if e.is_unique_violation() => {
                AdminError::Conflict { message: "resource already exists".to_string() }
            }
            Some(_) => AdminError::Conflict { message: "resource is still referenced".to_string() },
            None => AdminError::Unavailable,
        }
    }
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_type(), self.message())
    }
}

impl std::error::Error for AdminError {}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (self.status(), Json(self.body())).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbErrorKind;

    #[test]
    fn admin_error_bodies() {
        let b = AdminError::Unauthorized.body();
        assert_eq!(b, json!({"error":{"type":"unauthorized","message":"admin authentication failed"}}));
        let b = AdminError::validation("name", "bad").body();
        assert_eq!(b["error"]["field"], "name");
        assert_eq!(b["error"]["type"], "invalid_request");
        assert!(AdminError::Unavailable.body()["error"].get("field").is_none());
    }

    #[test]
    fn db_error_mapping() {
        let e: AdminError = DbError::with_sqlstate("23505", DbErrorKind::Other).into();
        assert_eq!(e.status(), StatusCode::CONFLICT);
        let e: AdminError = DbError::with_sqlstate("23503", DbErrorKind::Other).into();
        assert_eq!(e.status(), StatusCode::CONFLICT);
        let e: AdminError = DbError::new(DbErrorKind::Connection).into();
        assert_eq!(e.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn gateway_status_codes() {
        assert_eq!(GatewayError::ModelNotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            GatewayError::UpstreamExhausted { last_reason: FailoverReason::Http429 }.status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(GatewayError::AllBreakersOpen.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(GatewayError::RateLimited(RateLimitError::Rpm { retry_after: 3 }).status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn gateway_error_messages_are_non_empty() {
        assert_eq!(GatewayError::BadRequest("field 'model' is required".into()).message(), "field 'model' is required");
        assert!(!GatewayError::Unauthorized.message().is_empty());
        assert!(!GatewayError::Internal.message().is_empty());
    }

    #[test]
    fn upstream_rejected_falls_back_when_message_missing() {
        let e = GatewayError::upstream_rejected(StatusCode::NOT_FOUND, None);
        assert_eq!(e.message(), "upstream returned HTTP 404");
        let e = GatewayError::upstream_rejected(StatusCode::NOT_FOUND, Some("   ".into()));
        assert_eq!(e.message(), "upstream returned HTTP 404");
        let e = GatewayError::upstream_rejected(StatusCode::NOT_FOUND, Some("model x not found".into()));
        assert_eq!(e.message(), "model x not found");
    }

    #[test]
    fn error_response_body_matches_mapping_table() {
        let cases: &[(u16, &str, Option<&str>, &str)] = &[
            (400, "invalid_request_error", None, "invalid_request_error"),
            (401, "invalid_request_error", Some("invalid_api_key"), "authentication_error"),
            (403, "permission_error", None, "permission_error"),
            (404, "invalid_request_error", Some("model_not_found"), "not_found_error"),
            (429, "rate_limit_error", Some("rate_limit_exceeded"), "rate_limit_error"),
            (502, "api_error", None, "api_error"),
            (503, "api_error", None, "overloaded_error"),
            (418, "invalid_request_error", None, "invalid_request_error"),
            (500, "api_error", None, "api_error"),
        ];
        for (status, oa_type, oa_code, an_type) in cases {
            let status = StatusCode::from_u16(*status).unwrap();
            let oa = ErrorResponse::body(ClientProtocol::OpenAi, status, "boom");
            let expected_code = match oa_code {
                Some(c) => Value::String((*c).to_string()),
                None => Value::Null,
            };
            assert_eq!(oa, json!({"error": {"message": "boom", "type": oa_type, "code": expected_code}}));

            let an = ErrorResponse::body(ClientProtocol::Anthropic, status, "boom");
            assert_eq!(an, json!({"type": "error", "error": {"type": an_type, "message": "boom"}}));
        }
    }

    #[test]
    fn error_response_render_sets_status_and_content_type() {
        let resp = ErrorResponse::render(ClientProtocol::OpenAi, StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn scrub_redacts_all_occurrences() {
        let secret = SecretString::from("sk-upstream-abc123".to_string());
        let msg = "upstream rejected key sk-upstream-abc123 twice: sk-upstream-abc123";
        let scrubbed = scrub(msg, &secret);
        assert!(!scrubbed.contains("sk-upstream-abc123"));
        assert_eq!(scrubbed, "upstream rejected key [REDACTED] twice: [REDACTED]");
    }

    #[test]
    fn scrub_is_noop_without_match() {
        let secret = SecretString::from("sk-upstream-abc123".to_string());
        assert_eq!(scrub("no secret here", &secret), "no secret here");
    }
}
