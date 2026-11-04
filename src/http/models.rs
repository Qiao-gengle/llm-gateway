//! `GET /v1/models`（需求 4）。Client_Key 鉴权与 allowed_models 过滤在任务 12.6 接入；
//! 此前不做过滤（`allowed_models = None`）。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde_json::{Value, json};

use crate::registry::{Registry, Snapshot};

/// 构造 OpenAI 格式的模型列表响应体。
pub fn models_body(snapshot: &Snapshot, allowed_models: Option<&[String]>) -> Value {
    let data: Vec<Value> = snapshot
        .visible_models(allowed_models)
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "llm-gateway"}))
        .collect();
    json!({"object": "list", "data": data})
}

async fn list_models(State(registry): State<Arc<Registry>>) -> Json<Value> {
    Json(models_body(&registry.snapshot(), None))
}

pub fn router(registry: Arc<Registry>) -> Router {
    Router::new().route("/v1/models", get(list_models)).with_state(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Fingerprint, RouteEntry, RouteTarget};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn route(model: &str, enabled: &[bool]) -> (String, Arc<RouteEntry>) {
        let targets = enabled
            .iter()
            .map(|&e| RouteTarget {
                id: Uuid::new_v4(),
                provider_id: Uuid::new_v4(),
                upstream_model: "u".into(),
                priority: 0,
                weight: 1,
                default_max_tokens: None,
                enabled: e,
            })
            .collect();
        (model.into(), Arc::new(RouteEntry { id: Uuid::new_v4(), public_model: model.into(), targets }))
    }

    fn snap() -> Snapshot {
        let routes: HashMap<_, _> = [
            route("b", &[true]),
            route("a", &[false, true]),
            route("off", &[false]),
            route("empty", &[]),
        ]
        .into_iter()
        .collect();
        Snapshot { providers: HashMap::new(), routes_by_model: routes, fingerprint: Fingerprint::default() }
    }

    #[test]
    fn sorted_and_filters_disabled() {
        let body = models_body(&snap(), None);
        assert_eq!(
            body,
            json!({"object":"list","data":[
                {"id":"a","object":"model","owned_by":"llm-gateway"},
                {"id":"b","object":"model","owned_by":"llm-gateway"}]})
        );
    }

    #[test]
    fn allowed_models_intersects_and_ignores_unknown() {
        let allowed = vec!["b".to_string(), "off".to_string(), "missing".to_string()];
        assert_eq!(snap().visible_models(Some(&allowed)), vec!["b"]);
        assert_eq!(models_body(&snap(), Some(&[])), json!({"object":"list","data":[]}));
    }
}
