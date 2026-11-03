//! `GET /healthz`：存活探针，不访问任何依赖（需求 5.7）。

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::{Value, json};

pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok"}))
}
