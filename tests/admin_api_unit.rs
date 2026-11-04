//! Admin_API 单元测试（任务 2.12）：只经公开 Router，无需数据库（惰性连接池指向不可达地址）。
//!
//! **Validates: Requirements 8.1, 9.5, 9.9, 10.5**

use std::sync::Arc;
use std::time::Duration;

use llm_gateway::clock::OsRandom;
use llm_gateway::http::admin::{AdminState, admin_router};
use llm_gateway::secret::SecretStore;
use secrecy::SecretString;
use serde_json::{Value, json};

async fn spawn() -> String {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://u:p@127.0.0.1:1/db")
        .unwrap();
    let secrets = Arc::new(SecretStore::new(&[7u8; 32], Arc::new(OsRandom)));
    let app = admin_router(AdminState::new(&SecretString::from("tok")).with_db(pool, secrets));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn call(base: &str, method: &str, path: &str, auth: bool, body: &str) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .body(body.to_string());
    if auth {
        req = req.header("authorization", "Bearer tok");
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

const ID: &str = "00000000-0000-0000-0000-000000000000";

/// 需求 8.1：无 token 时，非法请求体、非法路径参数都返回 401，而不是 400/404。
#[tokio::test]
async fn missing_token_yields_401_before_any_validation() {
    let base = spawn().await;
    let cases = [
        ("POST", "/admin/providers".to_string(), "{bad"),
        ("POST", "/admin/providers".to_string(), "[]"),
        ("POST", "/admin/routes".to_string(), "{bad"),
        ("PATCH", format!("/admin/providers/{ID}"), "{bad"),
        ("PATCH", format!("/admin/routes/{ID}"), "{bad"),
        ("PATCH", "/admin/providers/not-a-uuid".to_string(), "{bad"),
        ("GET", "/admin/routes/not-a-uuid".to_string(), ""),
        ("DELETE", "/admin/providers/not-a-uuid".to_string(), ""),
    ];
    let mut first: Option<Value> = None;
    for (m, p, b) in cases {
        let (s, body) = call(&base, m, &p, false, b).await;
        assert_eq!(s, 401, "{m} {p}");
        match &first {
            None => first = Some(body),
            Some(f) => assert_eq!(&body, f, "401 body must not vary: {m} {p}"),
        }
    }
}

/// 需求 9.9：Provider 各字段校验错误的 `field`；在访问数据库之前返回 400。
#[tokio::test]
async fn provider_validation_errors_name_the_field() {
    let base = spawn().await;
    let ok = json!({"name": "p", "protocol": "openai", "base_url": "https://h.example", "api_key": "k"});
    let with = |k: &str, v: Value| {
        let mut o = ok.clone();
        o[k] = v;
        o
    };
    let mut cases: Vec<(Value, &str)> = vec![
        (with("name", json!("")), "name"),
        (with("name", json!("a".repeat(65))), "name"),
        (with("name", json!(5)), "name"),
        (with("protocol", json!("gemini")), "protocol"),
        (with("base_url", json!("ftp://h")), "base_url"),
        (with("base_url", json!("not a url")), "base_url"),
        (with("api_key", json!("")), "api_key"),
        (with("api_key", json!("k".repeat(4097))), "api_key"),
        (with("connect_timeout", json!(0)), "connect_timeout"),
        (with("ttfb_timeout", json!(-1)), "ttfb_timeout"),
        (with("total_timeout", json!("5")), "total_timeout"),
        (with("enabled", json!("yes")), "enabled"),
        // 超时顺序约束
        (with("connect_timeout", json!(601)), "connect_timeout"),
        (with("ttfb_timeout", json!(601)), "ttfb_timeout"),
    ];
    for f in ["name", "protocol", "base_url", "api_key"] {
        let mut o = ok.clone();
        o.as_object_mut().unwrap().remove(f);
        cases.push((o, f)); // 创建时缺少必填字段
    }
    for (body, field) in cases {
        let (s, b) = call(&base, "POST", "/admin/providers", true, &body.to_string()).await;
        assert_eq!(s, 400, "{body}");
        assert_eq!(b["error"]["field"].as_str(), Some(field), "{body}");
    }
    // 校验通过后才会访问数据库
    let (s, _) = call(&base, "POST", "/admin/providers", true, &ok.to_string()).await;
    assert_eq!(s, 503);
}

/// 需求 9.5：PATCH 中“未出现”的字段保持不变（校验通过并进入数据库阶段），
/// 显式 `null` 视为非法值而不是“清除/忽略”。
#[tokio::test]
async fn provider_patch_absent_vs_explicit_null() {
    let base = spawn().await;
    let path = format!("/admin/providers/{ID}");
    // 空对象与仅含部分字段：校验通过，到达数据库（不可达 → 503）
    for body in ["{}", r#"{"enabled":false}"#, r#"{"name":"x"}"#] {
        let (s, _) = call(&base, "PATCH", &path, true, body).await;
        assert_eq!(s, 503, "{body}");
    }
    // 显式 null：每个字段都应 400 并指出该字段
    for f in [
        "name", "protocol", "base_url", "api_key", "connect_timeout", "ttfb_timeout", "total_timeout", "enabled",
    ] {
        let body = format!(r#"{{"{f}":null}}"#);
        let (s, b) = call(&base, "PATCH", &path, true, &body).await;
        assert_eq!(s, 400, "{body}");
        assert_eq!(b["error"]["field"].as_str(), Some(f), "{body}");
    }
    let (s, b) = call(&base, "PATCH", &path, true, "[]").await;
    assert_eq!((s, b["error"]["field"].as_str()), (400, Some("body")));
}

fn target() -> Value {
    json!({"provider": ID, "upstream_model": "gpt", "priority": 0, "weight": 1})
}

fn target_with(k: &str, v: Value) -> Value {
    let mut t = target();
    t[k] = v;
    t
}

/// 需求 10.5：Route 各字段（含 `targets[i].xxx` 路径）校验错误的 `field`。
#[tokio::test]
async fn route_validation_errors_name_the_field() {
    let base = spawn().await;
    let cases: Vec<(Value, &str)> = vec![
        (json!({"targets": [target()]}), "public_model"),
        (json!({"public_model": "m"}), "targets"),
        (json!({"public_model": "", "targets": [target()]}), "public_model"),
        (json!({"public_model": "m".repeat(257), "targets": [target()]}), "public_model"),
        (json!({"public_model": "m", "targets": []}), "targets"),
        (json!({"public_model": "m", "targets": vec![target(); 33]}), "targets"),
        (json!({"public_model": "m", "targets": {}}), "targets"),
        (json!({"public_model": "m", "targets": [1]}), "targets[0]"),
        (json!({"public_model": "m", "targets": [target_with("provider", json!("x"))]}), "targets[0].provider"),
        (json!({"public_model": "m", "targets": [target_with("upstream_model", json!(""))]}), "targets[0].upstream_model"),
        (json!({"public_model": "m", "targets": [target_with("priority", json!(1001))]}), "targets[0].priority"),
        (json!({"public_model": "m", "targets": [target_with("weight", json!(0))]}), "targets[0].weight"),
        (json!({"public_model": "m", "targets": [target_with("weight", json!(10001))]}), "targets[0].weight"),
        (json!({"public_model": "m", "targets": [target_with("default_max_tokens", json!(0))]}), "targets[0].default_max_tokens"),
        (json!({"public_model": "m", "targets": [target_with("enabled", json!(1))]}), "targets[0].enabled"),
        (json!({"public_model": "m", "targets": [target(), target_with("weight", json!("1"))]}), "targets[1].weight"),
    ];
    for (body, field) in cases {
        let (s, b) = call(&base, "POST", "/admin/routes", true, &body.to_string()).await;
        assert_eq!(s, 400, "{body}");
        assert_eq!(b["error"]["field"].as_str(), Some(field), "{body}");
    }
    let (s, b) = call(&base, "POST", "/admin/routes", true, "{bad").await;
    assert_eq!((s, b["error"]["field"].as_str()), (400, Some("body")));
}

/// 需求 10.5：Route PATCH 的“未出现 / 显式 null”；`default_max_tokens` 的 null 在 target 内表示“无默认值”。
#[tokio::test]
async fn route_patch_absent_vs_explicit_null() {
    let base = spawn().await;
    let path = format!("/admin/routes/{ID}");
    for body in [
        "{}".to_string(),
        json!({"public_model": "x"}).to_string(),
        json!({"targets": [target()]}).to_string(),
        json!({"targets": [target_with("default_max_tokens", Value::Null)]}).to_string(),
    ] {
        let (s, _) = call(&base, "PATCH", &path, true, &body).await;
        assert_eq!(s, 503, "{body}");
    }
    for (body, field) in [
        (r#"{"public_model":null}"#, "public_model"),
        (r#"{"targets":null}"#, "targets"),
    ] {
        let (s, b) = call(&base, "PATCH", &path, true, body).await;
        assert_eq!(s, 400, "{body}");
        assert_eq!(b["error"]["field"].as_str(), Some(field), "{body}");
    }
}
