pub mod admin;
pub mod errors;
pub mod health;
pub mod ingress;
pub mod models;
pub mod request_id;

use std::sync::Arc;

use axum::Router;
use axum::middleware;

/// 组装 Router，并用 Request_ID 中间件包裹所有路由。
pub fn build_router() -> Router {
    let gen_ = Arc::new(request_id::RequestIdGenerator::new());
    Router::new()
        .merge(health::router())
        .layer(middleware::from_fn_with_state(
            gen_,
            request_id::request_id_middleware,
        ))
}
