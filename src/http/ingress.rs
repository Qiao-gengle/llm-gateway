//! `POST /v1/chat/completions`（非流式，需求 2、24）。
//!
//! 处理顺序（design.md“处理顺序”）：解析请求体（400）→ Registry 快照与 Route 检查（404）
//! → `ProxyEngine::execute` → `print_response`。Client_Key 鉴权、限流、流式在后续任务接入。

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use uuid::Uuid;

use crate::http::errors::{ClientProtocol as ErrProtocol, ErrorResponse, GatewayError};
use crate::http::request_id::RequestId;
use crate::ir::ClientProtocol;
use crate::protocol::{ResponseCtx, openai};
use crate::proxy::{ProxyEngine, ProxyRequest, ProxyResult};
use crate::registry::Registry;

#[derive(Clone)]
pub struct IngressState {
    pub registry: Arc<Registry>,
    pub engine: Arc<ProxyEngine>,
}

pub fn router(registry: Arc<Registry>) -> Router {
    let state = IngressState { registry, engine: Arc::new(ProxyEngine::new()) };
    Router::new().route("/v1/chat/completions", post(chat_completions)).with_state(state)
}

fn error_response(error: &GatewayError) -> Response {
    ErrorResponse::render(ErrProtocol::OpenAi, error.status(), &error.message())
}

async fn chat_completions(
    State(state): State<IngressState>,
    Extension(request_id): Extension<RequestId>,
    body: Bytes,
) -> Response {
    // 1. 解析请求体 → IR（400）。
    let ir = match openai::request::parse_request(&body) {
        Ok(ir) => ir,
        Err(e) => return error_response(&GatewayError::from(e)),
    };

    // 2. 快照与 Route（404）：Route 必须存在且至少有一个 enabled Route_Target。
    let snapshot = state.registry.snapshot();
    let route = match snapshot.routes_by_model.get(&ir.model) {
        Some(r) if r.targets.iter().any(|t| t.enabled) => r.clone(),
        _ => return error_response(&GatewayError::ModelNotFound),
    };

    // 3. 代理。
    let result = state
        .engine
        .execute(ProxyRequest {
            ir: &ir,
            client_protocol: ClientProtocol::OpenAi,
            stream: ir.stream,
            snapshot,
            route,
            request_id: &request_id,
        })
        .await;

    match result {
        ProxyResult::NonStream { ir: resp, .. } => {
            let ctx = ResponseCtx {
                id: format!("chatcmpl-{}", Uuid::new_v4()),
                created: chrono::Utc::now().timestamp(),
                public_model: ir.model.clone(),
            };
            (StatusCode::OK, axum::Json(openai::response::print_response(&resp, &ctx))).into_response()
        }
        ProxyResult::Failed { error, .. } => error_response(&error),
        // 流式在后续任务实现；引擎目前对流式请求返回 Failed。
        ProxyResult::Stream { .. } => error_response(&GatewayError::Internal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::build_router_full;
    use crate::registry::{ProviderEntry, RouteEntry, RouteTarget, Snapshot, Timeouts, UpstreamProtocol};
    use secrecy::SecretString;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};


    fn snapshot(base: &str, enabled: bool) -> Snapshot {
        let pid = Uuid::new_v4();
        let provider = ProviderEntry {
            id: pid,
            name: "p".into(),
            protocol: UpstreamProtocol::OpenAi,
            base_url: base.parse().unwrap(),
            api_key: Some(SecretString::from("sk-up".to_string())),
            timeouts: Timeouts {
                connect: Duration::from_secs(2),
                ttfb: Duration::from_secs(5),
                total: Duration::from_secs(10),
            },
            enabled: true,
        };
        let route = Arc::new(RouteEntry {
            id: Uuid::new_v4(),
            public_model: "pub".into(),
            targets: vec![RouteTarget {
                id: Uuid::new_v4(),
                provider_id: pid,
                upstream_model: "up".into(),
                priority: 0,
                weight: 1,
                default_max_tokens: None,
                enabled,
            }],
        });
        let mut s = Snapshot::empty();
        s.providers.insert(pid, provider);
        s.routes_by_model.insert("pub".into(), route);
        s
    }

    async fn spawn(snap: Snapshot) -> String {
        let reg = Arc::new(Registry::with_snapshot_for_test(snap));
        let app = build_router_full(None, None, Some(reg));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1/chat/completions")
    }

    async fn post_json(url: &str, body: serde_json::Value) -> reqwest::Response {
        reqwest::Client::new().post(url).json(&body).send().await.unwrap()
    }

    #[tokio::test]
    async fn invalid_body_is_400_openai_error() {
        let url = spawn(snapshot("http://127.0.0.1:1", true)).await;
        let resp = reqwest::Client::new().post(&url).body("{").send().await.unwrap();
        assert_eq!(resp.status(), 400);
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(!v["error"]["message"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_or_disabled_model_is_404() {
        let url = spawn(snapshot("http://127.0.0.1:1", false)).await;
        for model in ["pub", "missing"] {
            let resp = post_json(&url, serde_json::json!({"model": model, "messages": [{"role":"user","content":"hi"}]})).await;
            assert_eq!(resp.status(), 404);
            let v: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(v["error"]["code"], "model_not_found");
        }
    }

    #[tokio::test]
    async fn success_returns_openai_response_with_public_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"x","choices":[{"index":0,"message":{"role":"assistant","content":"yo"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})))
            .mount(&server)
            .await;
        let url = spawn(snapshot(&server.uri(), true)).await;
        let resp = post_json(&url, serde_json::json!({"model":"pub","messages":[{"role":"user","content":"hi"}]})).await;
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("x-request-id").is_some());
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["model"], "pub");
        assert_eq!(v["choices"][0]["message"]["content"], "yo");
        assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
    }

    #[tokio::test]
    async fn upstream_failure_is_502() {
        let url = spawn(snapshot("http://127.0.0.1:1", true)).await;
        let resp = post_json(&url, serde_json::json!({"model":"pub","messages":[{"role":"user","content":"hi"}]})).await;
        assert_eq!(resp.status(), 502);
    }
}
