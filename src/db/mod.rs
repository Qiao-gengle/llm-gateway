//! 数据库层。当前只包含错误类型；其余由后续任务实现。

use std::fmt;

use axum::http::StatusCode;

/// Postgres 唯一约束冲突。
pub const SQLSTATE_UNIQUE_VIOLATION: &str = "23505";
/// Postgres 外键约束冲突。
pub const SQLSTATE_FOREIGN_KEY_VIOLATION: &str = "23503";

/// 数据库错误类别（不含任何查询文本、参数或服务器返回的消息）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorKind {
    Connection,
    Timeout,
    Protocol,
    Decode,
    Migration,
    Other,
}

impl DbErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DbErrorKind::Connection => "connection",
            DbErrorKind::Timeout => "timeout",
            DbErrorKind::Protocol => "protocol",
            DbErrorKind::Decode => "decode",
            DbErrorKind::Migration => "migration",
            DbErrorKind::Other => "other",
        }
    }
}

/// 数据库错误。`Display` 只输出 SQLSTATE 与错误类别，避免泄露 SQL、参数和行数据（需求 34 第 6 条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbError {
    sqlstate: Option<String>,
    kind: DbErrorKind,
}

impl DbError {
    pub fn new(kind: DbErrorKind) -> Self {
        Self { sqlstate: None, kind }
    }

    pub fn with_sqlstate(sqlstate: impl Into<String>, kind: DbErrorKind) -> Self {
        Self { sqlstate: Some(sqlstate.into()), kind }
    }

    pub fn sqlstate(&self) -> Option<&str> {
        self.sqlstate.as_deref()
    }

    pub fn kind(&self) -> DbErrorKind {
        self.kind
    }

    pub fn is_unique_violation(&self) -> bool {
        self.sqlstate() == Some(SQLSTATE_UNIQUE_VIOLATION)
    }

    pub fn is_foreign_key_violation(&self) -> bool {
        self.sqlstate() == Some(SQLSTATE_FOREIGN_KEY_VIOLATION)
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.sqlstate {
            Some(s) => write!(f, "database error: sqlstate={} kind={}", s, self.kind.as_str()),
            None => write!(f, "database error: kind={}", self.kind.as_str()),
        }
    }
}

impl std::error::Error for DbError {}

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::Database(db) => {
                let code = db.code().map(|c| c.into_owned());
                DbError { sqlstate: code, kind: DbErrorKind::Other }
            }
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => {
                DbError::new(DbErrorKind::Connection)
            }
            sqlx::Error::PoolTimedOut => DbError::new(DbErrorKind::Timeout),
            sqlx::Error::Protocol(_) => DbError::new(DbErrorKind::Protocol),
            sqlx::Error::ColumnDecode { .. }
            | sqlx::Error::Decode(_)
            | sqlx::Error::ColumnNotFound(_)
            | sqlx::Error::ColumnIndexOutOfBounds { .. }
            | sqlx::Error::TypeNotFound { .. } => DbError::new(DbErrorKind::Decode),
            sqlx::Error::Migrate(_) => DbError::new(DbErrorKind::Migration),
            _ => DbError::new(DbErrorKind::Other),
        }
    }
}

/// 约束冲突映射：23505（唯一）与 23503（外键）→ 409，其他返回 `None`。
pub fn conflict_status(sqlstate: &str) -> Option<StatusCode> {
    match sqlstate {
        SQLSTATE_UNIQUE_VIOLATION | SQLSTATE_FOREIGN_KEY_VIOLATION => Some(StatusCode::CONFLICT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_constraint_violations_to_409() {
        assert_eq!(conflict_status("23505"), Some(StatusCode::CONFLICT));
        assert_eq!(conflict_status("23503"), Some(StatusCode::CONFLICT));
        assert_eq!(conflict_status("42P01"), None);
    }

    #[test]
    fn display_has_only_sqlstate_and_kind() {
        let e = DbError::with_sqlstate("23505", DbErrorKind::Other);
        assert_eq!(e.to_string(), "database error: sqlstate=23505 kind=other");
        assert!(e.is_unique_violation());
        let e = DbError::new(DbErrorKind::Connection);
        assert_eq!(e.to_string(), "database error: kind=connection");
    }
}
