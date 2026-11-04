//! Proxy_Engine（见 design.md“Proxy_Engine”，需求 17、18、24）。
//!
//! 初版只做单次尝试：选取 Route 中第一个 enabled 且 Provider 可用的 OpenAI Route_Target，
//! 发送一次非流式请求。重试、加权选择、熔断与流式在后续任务中补充。

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::stream::BoxStream;
use reqwest::StatusCode;
use secrecy::ExposeSecret;

use crate::http::errors::{FailoverReason, GatewayError, scrub};
use crate::http::request_id::RequestId;
use crate::ir::{ClientProtocol, IrRequest, IrResponse, IrStreamEvent, StreamErrorInfo};
use crate::protocol::openai;
use crate::registry::{ProviderEntry, ProviderId, RouteEntry, RouteTarget, Snapshot, UpstreamProtocol};

/// 流式响应中的一项：解码后的 IR 事件，或流内故障。
pub type StreamItem = Result<IrStreamEvent, StreamErrorInfo>;

pub struct ProxyRequest<'a> {
    pub ir: &'a IrRequest,
    pub client_protocol: ClientProtocol,
    pub stream: bool,
    pub snapshot: Arc<Snapshot>,
    pub route: Arc<RouteEntry>,
    pub request_id: &'a RequestId,
}

pub enum ProxyResult {
    NonStream { ir: IrResponse, meta: AttemptMeta },
    Stream { first: Vec<IrStreamEvent>, rest: BoxStream<'static, StreamItem>, meta: AttemptMeta },
    /// Bytes_Sent 之前的最终错误。
    Failed { error: GatewayError, meta: AttemptMeta },
}

#[derive(Debug, Clone, Default)]
pub struct AttemptMeta {
    pub attempts: u32,
    pub last_provider: Option<(ProviderId, String)>,
    pub last_upstream_model: Option<String>,
    pub ttfb: Option<Duration>,
}

#[derive(Default)]
pub struct ProxyEngine {
    /// 按 (Provider id, connect 超时) 缓存的 HTTP 客户端。
    clients: DashMap<(ProviderId, Duration), reqwest::Client>,
}

enum AttemptError {
    Retryable(FailoverReason),
    Rejected(GatewayError),
}

impl ProxyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    fn client_for(&self, p: &ProviderEntry) -> Result<reqwest::Client, reqwest::Error> {
        let key = (p.id, p.timeouts.connect);
        if let Some(c) = self.clients.get(&key) {
            return Ok(c.clone());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(p.timeouts.connect)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(self.clients.entry(key).or_insert(client).clone())
    }

    /// 首个 enabled 且 Provider 可用的 OpenAI 目标。
    fn first_candidate<'a>(
        snapshot: &'a Snapshot,
        route: &'a RouteEntry,
    ) -> Option<(&'a RouteTarget, &'a ProviderEntry)> {
        route.targets.iter().find_map(|t| {
            let p = snapshot.providers.get(&t.provider_id)?;
            (t.enabled && p.is_available() && p.protocol == UpstreamProtocol::OpenAi).then_some((t, p))
        })
    }

    pub async fn execute(&self, req: ProxyRequest<'_>) -> ProxyResult {
        let mut meta = AttemptMeta::default();
        let fail = |error: GatewayError, meta: AttemptMeta| ProxyResult::Failed { error, meta };

        if req.stream {
            // 流式在后续任务实现。
            return fail(GatewayError::Internal, meta);
        }
        let Some((target, provider)) = Self::first_candidate(&req.snapshot, &req.route) else {
            return fail(GatewayError::AllBreakersOpen, meta);
        };

        let printed = match openai::request::print_request(req.ir, target) {
            Ok(p) => p,
            Err(e) => return fail(GatewayError::BadRequest(e.to_string()), meta),
        };
        if !printed.dropped_extra.is_empty() {
            tracing::debug!(request_id = %req.request_id, dropped = ?printed.dropped_extra, "dropped extra fields");
        }
        let client = match self.client_for(provider) {
            Ok(c) => c,
            Err(_) => return fail(GatewayError::Internal, meta),
        };

        meta.attempts = 1;
        meta.last_provider = Some((provider.id, provider.name.clone()));
        meta.last_upstream_model = Some(target.upstream_model.clone());

        let started = Instant::now();
        let outcome = tokio::time::timeout(
            provider.timeouts.total,
            Self::send_once(&client, provider, &printed.body, started, &mut meta),
        )
        .await
        .unwrap_or(Err(AttemptError::Retryable(FailoverReason::Timeout)));

        match outcome {
            Ok(ir) => ProxyResult::NonStream { ir, meta },
            Err(AttemptError::Rejected(error)) => fail(error, meta),
            Err(AttemptError::Retryable(last_reason)) => {
                tracing::warn!(
                    request_id = %req.request_id, provider = %provider.name, reason = last_reason.as_str(),
                    "upstream attempt failed"
                );
                fail(GatewayError::UpstreamExhausted { last_reason }, meta)
            }
        }
    }

    async fn send_once(
        client: &reqwest::Client,
        provider: &ProviderEntry,
        body: &serde_json::Value,
        started: Instant,
        meta: &mut AttemptMeta,
    ) -> Result<IrResponse, AttemptError> {
        let Some(key) = provider.api_key.as_ref() else {
            return Err(AttemptError::Rejected(GatewayError::Internal));
        };
        let url = openai::request::upstream_url(provider.base_url.as_str());
        // 只带上游鉴权头，不转发 Client 的任何认证头。
        let send = client
            .post(url)
            .bearer_auth(key.expose_secret())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(body)
            .send();
        let resp = match tokio::time::timeout(provider.timeouts.ttfb, send).await {
            Err(_) => return Err(AttemptError::Retryable(FailoverReason::Timeout)),
            Ok(Err(e)) => {
                return Err(AttemptError::Retryable(if e.is_timeout() {
                    FailoverReason::Timeout
                } else {
                    FailoverReason::Connect
                }));
            }
            Ok(Ok(r)) => r,
        };
        meta.ttfb = Some(started.elapsed());

        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| {
            AttemptError::Retryable(if e.is_timeout() { FailoverReason::Timeout } else { FailoverReason::Connect })
        });

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(AttemptError::Retryable(FailoverReason::Http429));
        }
        if status.is_server_error() {
            return Err(AttemptError::Retryable(FailoverReason::Http5xx));
        }
        if status.is_client_error() {
            let message = bytes.ok().and_then(|b| upstream_error_message(&b)).map(|m| scrub(&m, key));
            return Err(AttemptError::Rejected(GatewayError::upstream_rejected(status, message)));
        }
        if !status.is_success() {
            // 1xx/3xx：非预期，按无效响应处理。
            return Err(AttemptError::Retryable(FailoverReason::InvalidResponse));
        }
        let bytes = bytes?;
        openai::response::parse_response(&bytes)
            .map_err(|_| AttemptError::Retryable(FailoverReason::InvalidResponse))
    }
}

/// 取上游错误体的 `error.message`（OpenAI 与 Anthropic 格式均在该路径）。
fn upstream_error_message(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("error")?.get("message")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Extra, IrMessage, IrBlock, IrRole, Sampling};
    use crate::registry::Timeouts;
    use secrecy::SecretString;
    use uuid::Uuid;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ir_request() -> IrRequest {
        IrRequest {
            model: "pub".into(),
            system: None,
            messages: vec![IrMessage { role: IrRole::User, content: vec![IrBlock::Text("hi".into())] }],
            sampling: Sampling::default(),
            tools: vec![],
            tool_choice: None,
            parallel_tool_calls: None,
            stream: false,
            include_usage: false,
            extra: Extra::default(),
        }
    }

    fn setup(base: &str, enabled_target: bool) -> (Arc<Snapshot>, Arc<RouteEntry>) {
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
                upstream_model: "up-model".into(),
                priority: 0,
                weight: 1,
                default_max_tokens: None,
                enabled: enabled_target,
            }],
        });
        let mut snap = Snapshot::empty();
        snap.providers.insert(pid, provider);
        (Arc::new(snap), route)
    }

    async fn run(server: &MockServer, template: ResponseTemplate) -> ProxyResult {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-up"))
            .respond_with(template)
            .mount(server)
            .await;
        let (snapshot, route) = setup(&server.uri(), true);
        let ir = ir_request();
        let rid = RequestId("r".into());
        ProxyEngine::new()
            .execute(ProxyRequest {
                ir: &ir,
                client_protocol: ClientProtocol::OpenAi,
                stream: false,
                snapshot,
                route,
                request_id: &rid,
            })
            .await
    }

    fn ok_body() -> serde_json::Value {
        serde_json::json!({"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":"yo"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
    }

    #[tokio::test]
    async fn success_returns_ir_and_meta() {
        let s = MockServer::start().await;
        match run(&s, ResponseTemplate::new(200).set_body_json(ok_body())).await {
            ProxyResult::NonStream { meta, .. } => {
                assert_eq!(meta.attempts, 1);
                assert_eq!(meta.last_upstream_model.as_deref(), Some("up-model"));
                assert!(meta.ttfb.is_some());
            }
            _ => panic!("expected NonStream"),
        }
    }

    #[tokio::test]
    async fn non_429_4xx_is_rejected_with_scrubbed_message() {
        let s = MockServer::start().await;
        let t = ResponseTemplate::new(400)
            .set_body_json(serde_json::json!({"error":{"message":"bad key sk-up"}}));
        match run(&s, t).await {
            ProxyResult::Failed { error: GatewayError::UpstreamRejected { status, message }, .. } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert!(!message.contains("sk-up"));
            }
            _ => panic!("expected UpstreamRejected"),
        }
    }

    #[tokio::test]
    async fn retryable_and_invalid_responses_map_to_exhausted() {
        for (t, reason) in [
            (ResponseTemplate::new(429), FailoverReason::Http429),
            (ResponseTemplate::new(503), FailoverReason::Http5xx),
            (ResponseTemplate::new(200).set_body_string("not json"), FailoverReason::InvalidResponse),
        ] {
            let s = MockServer::start().await;
            match run(&s, t).await {
                ProxyResult::Failed { error: GatewayError::UpstreamExhausted { last_reason }, meta } => {
                    assert_eq!(last_reason, reason);
                    assert_eq!(meta.attempts, 1);
                }
                _ => panic!("expected UpstreamExhausted"),
            }
        }
    }

    #[tokio::test]
    async fn connection_error_is_exhausted() {
        let (snapshot, route) = setup("http://127.0.0.1:1", true);
        let ir = ir_request();
        let rid = RequestId("r".into());
        let r = ProxyEngine::new()
            .execute(ProxyRequest {
                ir: &ir,
                client_protocol: ClientProtocol::OpenAi,
                stream: false,
                snapshot,
                route,
                request_id: &rid,
            })
            .await;
        assert!(matches!(
            r,
            ProxyResult::Failed { error: GatewayError::UpstreamExhausted { last_reason: FailoverReason::Connect }, .. }
        ));
    }

    #[tokio::test]
    async fn no_enabled_target_makes_no_attempt() {
        let (snapshot, route) = setup("http://127.0.0.1:1", false);
        let ir = ir_request();
        let rid = RequestId("r".into());
        let r = ProxyEngine::new()
            .execute(ProxyRequest {
                ir: &ir,
                client_protocol: ClientProtocol::OpenAi,
                stream: false,
                snapshot,
                route,
                request_id: &rid,
            })
            .await;
        match r {
            ProxyResult::Failed { meta, .. } => assert_eq!(meta.attempts, 0),
            _ => panic!("expected Failed"),
        }
    }
}
