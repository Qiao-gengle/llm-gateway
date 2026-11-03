//! `GET /healthz`（存活，不访问依赖，需求 5.7）与 `GET /readyz`（就绪，需求 5.2–5.4）。

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::BoxFuture;
use serde_json::{Value, json};

/// 单个依赖探测的超时上限（需求 5.2）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

type ProbeFn = dyn Fn() -> BoxFuture<'static, bool> + Send + Sync;

/// 一个具名依赖探测；返回 `true` 表示健康。
#[derive(Clone)]
pub struct Probe {
    name: &'static str,
    run: Arc<ProbeFn>,
}

impl Probe {
    pub fn new<F>(name: &'static str, run: F) -> Self
    where
        F: Fn() -> BoxFuture<'static, bool> + Send + Sync + 'static,
    {
        Self {
            name,
            run: Arc::new(run),
        }
    }

    /// Postgres 探测：`SELECT 1`。
    pub fn postgres(pool: sqlx::PgPool) -> Self {
        Self::new("postgres", move || {
            let pool = pool.clone();
            Box::pin(async move { sqlx::query("SELECT 1").execute(&pool).await.is_ok() })
        })
    }
}

/// 并发执行所有探测，每个带 `PROBE_TIMEOUT`；返回失败的依赖名（按探测列表顺序）。
pub async fn run_probes(probes: &[Probe]) -> Vec<&'static str> {
    let results = futures::future::join_all(probes.iter().map(|p| async move {
        let ok = tokio::time::timeout(PROBE_TIMEOUT, (p.run)())
            .await
            .unwrap_or(false);
        (p.name, ok)
    }))
    .await;
    results
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| n)
        .collect()
}

/// 仅 `/healthz`。
pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

/// `/readyz`，使用给定的依赖探测列表。
pub fn ready_router(probes: Vec<Probe>) -> Router {
    Router::new()
        .route("/readyz", get(readyz))
        .with_state(Arc::new(probes))
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn readyz(State(probes): State<Arc<Vec<Probe>>>) -> Response {
    let failed = run_probes(&probes).await;
    if failed.is_empty() {
        Json(json!({"status": "ok"})).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable", "failed": failed})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(name: &'static str, behavior: Option<bool>) -> Probe {
        // Some(b): 立即返回 b；None: 永久挂起。
        Probe::new(name, move || match behavior {
            Some(b) => Box::pin(async move { b }),
            None => Box::pin(futures::future::pending()),
        })
    }

    async fn get_ready(probes: Vec<Probe>) -> (StatusCode, Value) {
        let resp = readyz(State(Arc::new(probes))).await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn all_healthy_returns_200() {
        let (s, _) = get_ready(vec![probe("postgres", Some(true))]).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn failing_probe_listed_only() {
        let (s, body) = get_ready(vec![
            probe("postgres", Some(false)),
            probe("redis", Some(true)),
        ])
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({"status":"unavailable","failed":["postgres"]}));
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_probes_time_out_within_3s() {
        let start = tokio::time::Instant::now();
        let (s, body) = get_ready(vec![probe("postgres", None), probe("redis", None)]).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({"status":"unavailable","failed":["postgres","redis"]})
        );
        assert!(start.elapsed() <= Duration::from_secs(3));
    }
}
