pub mod admin;
pub mod errors;
pub mod health;
pub mod ingress;
pub mod models;
pub mod request_id;

use std::sync::Arc;

use axum::Router;
use axum::middleware;

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, build_router()).await.unwrap();
        });
        format!("http://{addr}/healthz")
    }

    #[tokio::test]
    async fn healthz_returns_200_without_dependencies() {
        let url = spawn().await;
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body, serde_json::json!({"status": "ok"}));
    }

    async fn ids_for(values: &[&str]) -> Vec<String> {
        let url = spawn().await;
        let mut req = reqwest::Client::new().get(&url);
        for v in values {
            req = req.header("x-request-id", *v);
        }
        let resp = req.send().await.unwrap();
        resp.headers()
            .get_all("x-request-id")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn valid_request_id_is_echoed_once() {
        assert_eq!(ids_for(&["abc-123"]).await, vec!["abc-123".to_owned()]);
    }

    #[tokio::test]
    async fn invalid_request_id_is_replaced_with_single_header() {
        let ids = ids_for(&["a b"]).await;
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], "a b");
        assert_eq!(ids[0].len(), 36);
    }

    #[tokio::test]
    async fn multiple_request_ids_yield_single_generated_header() {
        let ids = ids_for(&["first", "second"]).await;
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], "first");
        assert_ne!(ids[0], "second");
    }

    #[tokio::test]
    async fn missing_request_id_is_generated() {
        let ids = ids_for(&[]).await;
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].len(), 36);
    }
}

/// 在 `router` 上挂入 `/admin` 与 `/admin/*`（经 `admin_auth` 保护）。
pub fn with_admin(router: Router, state: admin::AdminState) -> Router {
    router.merge(admin::admin_router(state))
}

/// 组装 Router，并用 Request_ID 中间件包裹所有路由。
pub fn build_router() -> Router {
    build_router_with_admin(None, None)
}

/// 同 `build_router`，提供 `AdminState` 时挂入 Admin_API，提供探测列表时挂入 `/readyz`。
pub fn build_router_with_admin(
    admin: Option<admin::AdminState>,
    probes: Option<Vec<health::Probe>>,
) -> Router {
    build_router_full(admin, probes, None)
}

/// 同 `build_router_with_admin`，提供 Registry 时挂入 `GET /v1/models`。
pub fn build_router_full(
    admin: Option<admin::AdminState>,
    probes: Option<Vec<health::Probe>>,
    models: Option<Arc<crate::registry::Registry>>,
) -> Router {
    let gen_ = Arc::new(request_id::RequestIdGenerator::new());
    let mut router = Router::new().merge(health::router());
    if let Some(registry) = models {
        router = router.merge(models::router(registry));
    }
    if let Some(probes) = probes {
        router = router.merge(health::ready_router(probes));
    }
    let router = match admin {
        Some(state) => with_admin(router, state),
        None => router,
    };
    router
        .layer(middleware::from_fn_with_state(
            gen_,
            request_id::request_id_middleware,
        ))
}
