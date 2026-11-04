//! Admin_API：`/admin` 与 `/admin/*`，所有请求先经 `admin_auth`（需求 8）。

pub mod providers;
pub mod routes;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::http::errors::AdminError;
use crate::registry::Registry;
use crate::secret::SecretStore;

/// Admin_Token 的 SHA-256 摘要；构造时一次性读取明文，之后只保留摘要。
#[derive(Clone)]
pub struct AdminToken([u8; 32]);

impl AdminToken {
    pub fn new(token: &SecretString) -> Self {
        Self(Sha256::digest(token.expose_secret().as_bytes()).into())
    }

    /// 常量时间比较：对两侧做 SHA-256 后比较 32 字节摘要。
    fn matches(&self, candidate: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(candidate).into();
        self.0.ct_eq(&digest).into()
    }
}

impl std::fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminToken([REDACTED])")
    }
}

/// 管理接口的数据库与加密依赖。
#[derive(Clone)]
pub struct AdminDb {
    pub pool: sqlx::PgPool,
    pub secrets: Arc<SecretStore>,
    /// 写入提交后用于刷新快照；为 `None` 时不刷新（仅测试使用）。
    pub registry: Option<Arc<Registry>>,
}

impl AdminDb {
    /// 事务提交成功后调用；刷新失败只记 warn，不影响响应（需求 14.3、14.7）。
    pub async fn refresh_registry(&self) {
        if let Some(registry) = &self.registry {
            if let Err(e) = registry.reload().await {
                tracing::warn!(error = %e, "registry reload after admin write failed; keeping previous snapshot");
            }
        }
    }
}

impl std::fmt::Debug for AdminDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminDb").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct AdminState {
    pub admin_token: AdminToken,
    /// 为 `None` 时只挂鉴权与占位路由（不含 CRUD）。
    pub db: Option<AdminDb>,
}

impl AdminState {
    pub fn new(admin_token: &SecretString) -> Self {
        Self { admin_token: AdminToken::new(admin_token), db: None }
    }

    pub fn with_db(mut self, pool: sqlx::PgPool, secrets: Arc<SecretStore>) -> Self {
        self.db = Some(AdminDb { pool, secrets, registry: None });
        self
    }

    /// 设置写入后需要刷新的 Registry；须在 `with_db` 之后调用。
    pub fn with_registry(mut self, registry: Arc<Registry>) -> Self {
        if let Some(db) = self.db.as_mut() {
            db.registry = Some(registry);
        }
        self
    }
}

/// 从 `Authorization` 头提取 `Bearer <token>` 中的令牌；scheme 不区分大小写，
/// scheme 与令牌之间恰为一个空格，令牌不得为空。
fn extract_bearer(req: &Request) -> Option<&[u8]> {
    let value = req.headers().get(header::AUTHORIZATION)?.as_bytes();
    if value.len() < 8 || !value[..6].eq_ignore_ascii_case(b"bearer") || value[6] != b' ' {
        return None;
    }
    Some(&value[7..])
}

/// 鉴权中间件：在任何 extractor 之前运行，不读取请求体。
pub async fn admin_auth(State(token): State<AdminToken>, req: Request, next: Next) -> Response {
    match extract_bearer(&req) {
        Some(candidate) if token.matches(candidate) => next.run(req).await,
        _ => AdminError::Unauthorized.into_response(),
    }
}

async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// 挂在 `/admin`（含其本身）下的 Router；后续任务在 layer 之前添加具体路由。
pub fn admin_router(state: AdminState) -> Router {
    let mut router = Router::new();
    if let Some(db) = state.db {
        router = router.merge(providers::router(db.clone())).merge(routes::router(db));
    }
    router
        .route("/admin", any(not_found))
        .route("/admin/", any(not_found))
        .route("/admin/{*rest}", any(not_found))
        .layer(middleware::from_fn_with_state(state.admin_token, admin_auth))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn status_and_body(auth: Option<&str>, path: &str) -> (StatusCode, Vec<u8>, bool) {
        let app = admin_router(AdminState::new(&SecretString::from("s3cret-Token")));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut req = reqwest::Client::new().get(format!("http://{addr}{path}"));
        if let Some(a) = auth {
            req = req.header("authorization", a);
        }
        let resp = req.send().await.unwrap();
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let leaked = resp.headers().values().any(|v| v.as_bytes().windows(6).any(|w| w == b"s3cret"));
        let body = resp.bytes().await.unwrap().to_vec();
        (status, body, leaked)
    }

    #[tokio::test]
    async fn accepts_valid_token_case_insensitive_scheme() {
        for h in ["Bearer s3cret-Token", "bearer s3cret-Token", "BEARER s3cret-Token"] {
            let (s, _, _) = status_and_body(Some(h), "/admin/anything").await;
            assert_eq!(s, StatusCode::NOT_FOUND, "passed auth: {h}");
        }
        let (s, _, _) = status_and_body(Some("Bearer s3cret-Token"), "/admin").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_all_bad_credentials_identically() {
        let cases = [
            None,
            Some("Basic s3cret-Token"),
            Some("Bearer "),
            Some("Bearer"),
            Some("Bearer  s3cret-Token"),
            Some("Bearer s3cret-token"),
            Some("Bearer s3cret-Toke"),
            Some("Bearer s3cret-Tokenx"),
            Some("s3cret-Token"),
        ];
        let mut first: Option<Vec<u8>> = None;
        for c in cases {
            let (s, body, leaked) = status_and_body(c, "/admin/providers").await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{c:?}");
            assert!(!leaked);
            assert!(!String::from_utf8_lossy(&body).contains("s3cret"));
            match &first {
                None => first = Some(body),
                Some(f) => assert_eq!(&body, f),
            }
        }
    }
}
