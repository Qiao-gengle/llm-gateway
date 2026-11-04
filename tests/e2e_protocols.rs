//! 协议矩阵端到端测试（进程内网关 + wiremock 上游）。需要 `--features testkit`。
#![cfg(feature = "testkit")]

mod common;

use common::gateway::{RouteSpec, TestGateway};
use common::upstream::{openai_upstream, received, text_response, tool_call_response};
use serde_json::{Value, json};

async fn gateway_for(upstream_uri: String) -> TestGateway {
    TestGateway::start(vec![RouteSpec {
        public_model: "pub-model".into(),
        upstream_model: "up-model".into(),
        protocol: "openai",
        base_url: upstream_uri,
        upstream_key: "sk-upstream-secret".into(),
    }])
    .await
}

/// 除 `model` 外，上游请求体应与客户端请求体一致。
fn assert_same_except_model(client: &Value, upstream: &Value) {
    let (mut c, mut u) = (client.clone(), upstream.clone());
    assert_eq!(u["model"], "up-model");
    // 网关生成的字段：`model` 与显式的 `stream: false`。
    assert_eq!(u["stream"], false);
    for k in ["model", "stream"] {
        c.as_object_mut().unwrap().remove(k);
        u.as_object_mut().unwrap().remove(k);
    }
    assert_eq!(c, u);
}

/// 场景 1：OpenAI → OpenAI 非流式，纯文本。
#[tokio::test]
async fn openai_to_openai_text() {
    let up = openai_upstream(text_response("up-model", "hello back")).await;
    let gw = gateway_for(up.uri()).await;
    let body = json!({
        "model": "pub-model",
        "messages": [{"role": "system", "content": "be brief"}, {"role": "user", "content": "hi"}],
        "temperature": 0.5,
        "seed": 42,
        "response_format": {"type": "json_object"}
    });
    let resp = gw.post_json("/v1/chat/completions", &body).await;
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["model"], "pub-model");
    assert_eq!(v["choices"][0]["message"]["content"], "hello back");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["total_tokens"], 5);

    let (sent, auth) = received(&up, 0).await;
    assert_eq!(auth.as_deref(), Some("Bearer sk-upstream-secret"));
    assert_eq!(sent["seed"], 42);
    assert_same_except_model(&body, &sent);
}

/// 场景 1：OpenAI → OpenAI 非流式，工具调用。
#[tokio::test]
async fn openai_to_openai_tool_call() {
    let up = openai_upstream(tool_call_response("up-model")).await;
    let gw = gateway_for(up.uri()).await;
    let body = json!({
        "model": "pub-model",
        "messages": [{"role": "user", "content": "weather in Paris?"}],
        "tools": [{"type": "function", "function": {
            "name": "get_weather", "description": "weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
        }}],
        "tool_choice": "auto",
        "parallel_tool_calls": false
    });
    let resp = gw.post_json("/v1/chat/completions", &body).await;
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["model"], "pub-model");
    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
    let call = &v["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "call_1");
    assert_eq!(call["function"]["name"], "get_weather");
    let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args, json!({"city": "Paris"}));

    let (sent, auth) = received(&up, 0).await;
    assert_eq!(auth.as_deref(), Some("Bearer sk-upstream-secret"));
    assert_eq!(sent["parallel_tool_calls"], false);
    assert_same_except_model(&body, &sent);
}
