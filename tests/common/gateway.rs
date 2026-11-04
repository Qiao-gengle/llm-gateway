//! `TestGateway`：内存存储 + `127.0.0.1:0` 的进程内网关。

use std::sync::Arc;

use chrono::Utc;
use llm_gateway::clock::OsRandom;
use llm_gateway::http::build_router_full;
use llm_gateway::registry::{RawData, RawProvider, RawRoute, RawTarget, Registry};
use llm_gateway::secret::SecretStore;
use llm_gateway::testkit::InMemoryRegistryStore;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// 一个 Provider 及其唯一 Route 的描述。
pub struct RouteSpec {
    pub public_model: String,
    pub upstream_model: String,
    pub protocol: &'static str,
    pub base_url: String,
    pub upstream_key: String,
}

pub struct TestGateway {
    pub base_url: String,
    pub store: Arc<InMemoryRegistryStore>,
    pub registry: Arc<Registry>,
    handle: JoinHandle<()>,
}

impl TestGateway {
    pub async fn start(routes: Vec<RouteSpec>) -> TestGateway {
        let secrets = Arc::new(SecretStore::new(&[9u8; 32], Arc::new(OsRandom)));
        let now = Utc::now();
        let mut data = RawData::default();
        for spec in routes {
            let provider_id = Uuid::new_v4();
            let route_id = Uuid::new_v4();
            let sealed = secrets.seal(spec.upstream_key.as_bytes());
            data.providers.push(RawProvider {
                id: provider_id,
                name: format!("p-{}", spec.public_model),
                protocol: spec.protocol.into(),
                base_url: spec.base_url,
                api_key_nonce: sealed.nonce.to_vec(),
                api_key_ciphertext: sealed.ciphertext,
                connect_timeout_s: 2,
                ttfb_timeout_s: 10,
                total_timeout_s: 20,
                enabled: true,
                updated_at: now,
            });
            data.routes.push(RawRoute { id: route_id, public_model: spec.public_model, updated_at: now });
            data.targets.push(RawTarget {
                id: Uuid::new_v4(),
                route_id,
                position: 0,
                provider_id,
                upstream_model: spec.upstream_model,
                priority: 0,
                weight: 1,
                default_max_tokens: None,
                enabled: true,
                updated_at: now,
            });
        }
        let store = Arc::new(InMemoryRegistryStore::new(data));
        let registry = Arc::new(
            Registry::load_initial(store.clone(), secrets).await.expect("initial registry load"),
        );
        let app = build_router_full(None, None, Some(registry.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TestGateway { base_url: format!("http://{addr}"), store, registry, handle }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub async fn post_json(&self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        reqwest::Client::new().post(self.url(path)).json(body).send().await.unwrap()
    }
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
