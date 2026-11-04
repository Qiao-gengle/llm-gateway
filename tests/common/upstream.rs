//! wiremock 上游辅助。

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 启动一个对 OpenAI `POST /v1/chat/completions` 返回固定 JSON 的上游。
pub async fn openai_upstream(response: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(&server)
        .await;
    server
}

/// 取上游收到的第 `idx` 个请求的 JSON 体与 Authorization 头。
pub async fn received(server: &MockServer, idx: usize) -> (Value, Option<String>) {
    let reqs = server.received_requests().await.expect("request recording enabled");
    let r = &reqs[idx];
    let auth = r.headers.get("authorization").map(|v| v.to_str().unwrap().to_owned());
    (serde_json::from_slice(&r.body).unwrap(), auth)
}

pub fn text_response(upstream_model: &str, text: &str) -> Value {
    json!({
        "id": "up-1", "object": "chat.completion", "model": upstream_model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    })
}

pub fn tool_call_response(upstream_model: &str) -> Value {
    json!({
        "id": "up-2", "object": "chat.completion", "model": upstream_model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
        ]}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
}
