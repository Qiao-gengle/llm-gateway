//! 错误类型：请求阶段统一错误 `GatewayError`、校验/限流错误，以及 Admin_API 错误体。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::db::{DbError, conflict_status};

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
}
