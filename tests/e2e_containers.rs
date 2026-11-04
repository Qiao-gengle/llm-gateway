//! 依赖 Docker 的集成测试：真实 Postgres 上执行迁移与 Admin CRUD（任务 2.13）。
//!
//! 无 Docker 时每个测试以 `docker_or_skip!()` 开头并跳过。
//!
//! **Validates: Requirements 39.3, 39.4, 9.1, 9.7, 9.10, 9.11, 10.1, 10.6, 13.3**

#[macro_use]
mod common;

use std::sync::Arc;

use llm_gateway::clock::OsRandom;
use llm_gateway::db::connect_and_migrate;
use llm_gateway::http::admin::{AdminState, admin_router};
use llm_gateway::secret::{Sealed, SecretStore};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sqlx::PgPool;

const MASTER: [u8; 32] = [9u8; 32];
const UPSTREAM_KEY: &str = "sk-upstream-PLAINTEXT-k3y-9f2a";

struct Admin {
    base: String,
    pool: PgPool,
    http: reqwest::Client,
}

impl Admin {
    async fn start(pg_url: &str) -> Self {
        let pool = connect_and_migrate(pg_url).await.expect("connect and migrate");
        let secrets = Arc::new(SecretStore::new(&MASTER, Arc::new(OsRandom)));
        let app = admin_router(AdminState::new(&SecretString::from("tok")).with_db(pool.clone(), secrets));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base: format!("http://{addr}"), pool, http: reqwest::Client::new() }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value, String) {
        let mut req = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .header("authorization", "Bearer tok");
        if let Some(b) = body {
            req = req.header("content-type", "application/json").body(b.to_string());
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null), text)
    }

    async fn provider(&self, name: &str) -> String {
        let (s, b, _) = self
            .call(
                "POST",
                "/admin/providers",
                Some(json!({"name": name, "protocol": "openai", "base_url": "http://up.example/v1", "api_key": UPSTREAM_KEY})),
            )
            .await;
        assert_eq!(s, 201, "{b}");
        b["id"].as_str().unwrap().to_string()
    }
}

fn target(provider: &str, model: &str) -> Value {
    json!({"provider": provider, "upstream_model": model, "priority": 1, "weight": 10})
}

fn assert_no_key_material(text: &str) {
    for needle in [UPSTREAM_KEY, "api_key", "nonce", "ciphertext"] {
        assert!(!text.contains(needle), "response leaks {needle:?}: {text}");
    }
}

#[tokio::test]
async fn migration_is_idempotent_and_creates_tables() {
    let pg = docker_or_skip!();
    let pool = connect_and_migrate(&pg.url).await.expect("first migrate");
    drop(pool);
    let pool = connect_and_migrate(&pg.url).await.expect("second migrate");
    for t in ["providers", "routes", "route_targets"] {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM information_schema.tables WHERE table_name = $1")
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "table {t}");
    }
}

#[tokio::test]
async fn provider_crud_404_409_and_no_key_leak() {
    let pg = docker_or_skip!();
    let a = Admin::start(&pg.url).await;

    // 创建：默认超时 5/60/600、enabled=true；响应不含密钥材料
    let (s, b, text) = a
        .call(
            "POST",
            "/admin/providers",
            Some(json!({"name": "p1", "protocol": "openai", "base_url": "http://up.example/v1", "api_key": UPSTREAM_KEY})),
        )
        .await;
    assert_eq!(s, 201, "{b}");
    assert_no_key_material(&text);
    assert_eq!((b["connect_timeout"].as_i64(), b["ttfb_timeout"].as_i64(), b["total_timeout"].as_i64()), (Some(5), Some(60), Some(600)));
    assert_eq!(b["enabled"], true);
    let id = b["id"].as_str().unwrap().to_string();
    let created_at = b["created_at"].clone();

    // 409：name 重复
    let (s, b, _) = a
        .call(
            "POST",
            "/admin/providers",
            Some(json!({"name": "p1", "protocol": "anthropic", "base_url": "http://x.example", "api_key": "k"})),
        )
        .await;
    assert_eq!((s, b["error"]["type"].as_str()), (409, Some("conflict")));

    // 列表与查询
    let id2 = a.provider("p2").await;
    let (s, b, text) = a.call("GET", "/admin/providers", None).await;
    assert_eq!(s, 200);
    assert_no_key_material(&text);
    let names: Vec<&str> = b.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["p1", "p2"]);
    let (s, b, text) = a.call("GET", &format!("/admin/providers/{id}"), None).await;
    assert_eq!((s, b["name"].as_str()), (200, Some("p1")));
    assert_no_key_material(&text);

    // PATCH：未出现的字段不变；api_key 缺省时保留原密文
    let before: (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT api_key_nonce, api_key_ciphertext FROM providers WHERE id = $1")
            .bind(uid(&id))
            .fetch_one(&a.pool)
            .await
            .unwrap();
    let (s, b, text) = a
        .call("PATCH", &format!("/admin/providers/{id}"), Some(json!({"enabled": false, "ttfb_timeout": 30})))
        .await;
    assert_eq!(s, 200, "{b}");
    assert_no_key_material(&text);
    assert_eq!((b["enabled"].clone(), b["ttfb_timeout"].as_i64(), b["name"].as_str()), (json!(false), Some(30), Some("p1")));
    assert_eq!(b["created_at"], created_at);
    assert!(b["updated_at"].as_str().unwrap() > created_at.as_str().unwrap(), "updated_at must advance");
    let after: (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT api_key_nonce, api_key_ciphertext FROM providers WHERE id = $1")
            .bind(uid(&id))
            .fetch_one(&a.pool)
            .await
            .unwrap();
    assert_eq!(before, after, "key unchanged when api_key absent");

    // PATCH api_key：密文改变，且可解密为新明文
    let (s, _, text) = a.call("PATCH", &format!("/admin/providers/{id}"), Some(json!({"api_key": "new-key-value"}))).await;
    assert_eq!(s, 200);
    assert_no_key_material(&text);
    let (nonce, ct): (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT api_key_nonce, api_key_ciphertext FROM providers WHERE id = $1")
            .bind(uid(&id))
            .fetch_one(&a.pool)
            .await
            .unwrap();
    assert_ne!(ct, before.1);
    let store = SecretStore::new(&MASTER, Arc::new(OsRandom));
    let opened = store.open(&Sealed { nonce: nonce.try_into().unwrap(), ciphertext: ct }).unwrap();
    assert_eq!(opened.expose_secret(), "new-key-value");

    // PATCH 校验失败（超时顺序）→ 400 且带 field；name 冲突 → 409
    let (s, b, _) = a.call("PATCH", &format!("/admin/providers/{id}"), Some(json!({"total_timeout": 10}))).await;
    assert_eq!((s, b["error"]["field"].as_str()), (400, Some("ttfb_timeout")));
    let (s, _, _) = a.call("PATCH", &format!("/admin/providers/{id}"), Some(json!({"name": "p2"}))).await;
    assert_eq!(s, 409);

    // 404：不存在的 id 与非 UUID id
    let missing = "00000000-0000-0000-0000-000000000001";
    for m in ["GET", "DELETE"] {
        let (s, b, _) = a.call(m, &format!("/admin/providers/{missing}"), None).await;
        assert_eq!((s, b["error"]["type"].as_str()), (404, Some("not_found")), "{m}");
    }
    let (s, _, _) = a.call("PATCH", &format!("/admin/providers/{missing}"), Some(json!({"enabled": true}))).await;
    assert_eq!(s, 404);
    let (s, _, _) = a.call("GET", "/admin/providers/not-a-uuid", None).await;
    assert_eq!(s, 404);

    // 被 Route_Target 引用时删除 → 409；解除引用后 → 204，再次删除 → 404
    let (s, rb, _) = a
        .call("POST", "/admin/routes", Some(json!({"public_model": "m", "targets": [target(&id2, "up-m")]})))
        .await;
    assert_eq!(s, 201, "{rb}");
    let (s, b, _) = a.call("DELETE", &format!("/admin/providers/{id2}"), None).await;
    assert_eq!((s, b["error"]["type"].as_str()), (409, Some("conflict")));
    let (s, _, _) = a.call("DELETE", &format!("/admin/routes/{}", rb["id"].as_str().unwrap()), None).await;
    assert_eq!(s, 204);
    for expect in [204, 404] {
        let (s, _, _) = a.call("DELETE", &format!("/admin/providers/{id2}"), None).await;
        assert_eq!(s, expect);
    }

    // 需求 13.3：对 providers 表全部列检索 Upstream_Key 明文，得到 0 行。
    // 对照：同一查询能按 name 命中 1 行，证明检索方式有效。
    let hex = hex_of(UPSTREAM_KEY);
    let hits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM providers p WHERE p::text LIKE '%' || $1 || '%' \
         OR p::text LIKE '%' || $2 || '%' \
         OR position(convert_to($1, 'UTF8') in p.api_key_ciphertext) > 0 \
         OR position(convert_to($1, 'UTF8') in p.api_key_nonce) > 0 \
         OR position(convert_to($1, 'UTF8') in convert_to(p.name || p.base_url || p.protocol, 'UTF8')) > 0",
    )
    .bind(UPSTREAM_KEY)
    .bind(&hex)
    .fetch_one(&a.pool)
    .await
    .unwrap();
    assert_eq!(hits, 0, "Upstream_Key plaintext found in providers table");
    let control: i64 = sqlx::query_scalar("SELECT count(*) FROM providers p WHERE p::text LIKE '%' || $1 || '%'")
        .bind("p1")
        .fetch_one(&a.pool)
        .await
        .unwrap();
    assert_eq!(control, 1);
}

fn hex_of(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn route_crud_404_409_and_target_replacement() {
    let pg = docker_or_skip!();
    let a = Admin::start(&pg.url).await;
    let pa = a.provider("pa").await;
    let pb = a.provider("pb").await;

    // 创建：数组顺序即 position
    let (s, b, _) = a
        .call(
            "POST",
            "/admin/routes",
            Some(json!({"public_model": "gpt-x", "targets": [
                target(&pb, "m-b"),
                {"provider": pa, "upstream_model": "m-a", "priority": 0, "weight": 5, "default_max_tokens": 256, "enabled": false}
            ]})),
        )
        .await;
    assert_eq!(s, 201, "{b}");
    let rid = b["id"].as_str().unwrap().to_string();
    let models: Vec<&str> = b["targets"].as_array().unwrap().iter().map(|t| t["upstream_model"].as_str().unwrap()).collect();
    assert_eq!(models, ["m-b", "m-a"]);
    assert_eq!(b["targets"][1]["default_max_tokens"], 256);
    assert_eq!(b["targets"][1]["enabled"], false);
    assert_eq!(b["targets"][0]["default_max_tokens"], Value::Null);
    assert_eq!(b["targets"][0]["enabled"], true);

    // 409：public_model 重复
    let (s, b, _) = a
        .call("POST", "/admin/routes", Some(json!({"public_model": "gpt-x", "targets": [target(&pa, "z")]})))
        .await;
    assert_eq!((s, b["error"]["type"].as_str()), (409, Some("conflict")));

    // 400：Provider 不存在，field 指向 targets[i].provider
    let ghost = "00000000-0000-0000-0000-0000000000aa";
    let (s, b, _) = a
        .call("POST", "/admin/routes", Some(json!({"public_model": "other", "targets": [target(&pa, "z"), target(ghost, "z")]})))
        .await;
    assert_eq!((s, b["error"]["field"].as_str()), (400, Some("targets[1].provider")));
    // 失败的创建不留下半成品 Route
    let (_, b, _) = a.call("GET", "/admin/routes?public_model=other", None).await;
    assert_eq!(b.as_array().unwrap().len(), 0);

    // 列表、过滤、查询
    let (s, _, _) = a.call("POST", "/admin/routes", Some(json!({"public_model": "second", "targets": [target(&pa, "s")]}))).await;
    assert_eq!(s, 201);
    let (s, b, _) = a.call("GET", "/admin/routes", None).await;
    assert_eq!((s, b.as_array().unwrap().len()), (200, 2));
    let (_, b, _) = a.call("GET", "/admin/routes?public_model=second", None).await;
    assert_eq!(b.as_array().unwrap().len(), 1);
    assert_eq!(b[0]["public_model"], "second");
    let (_, b, _) = a.call("GET", "/admin/routes?public_model=SECOND", None).await;
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, b, _) = a.call("GET", &format!("/admin/routes/{rid}"), None).await;
    assert_eq!((s, b["targets"].as_array().unwrap().len()), (200, 2));

    // PATCH 只改 public_model：Route_Target 保持不变且顺序不变
    let (s, b, _) = a.call("PATCH", &format!("/admin/routes/{rid}"), Some(json!({"public_model": "gpt-y"}))).await;
    assert_eq!((s, b["public_model"].as_str()), (200, Some("gpt-y")));
    let models: Vec<&str> = b["targets"].as_array().unwrap().iter().map(|t| t["upstream_model"].as_str().unwrap()).collect();
    assert_eq!(models, ["m-b", "m-a"]);

    // PATCH targets：整体替换，顺序按提交
    let (s, b, _) = a
        .call(
            "PATCH",
            &format!("/admin/routes/{rid}"),
            Some(json!({"targets": [target(&pa, "n1"), target(&pb, "n2"), target(&pa, "n3")]})),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    let models: Vec<&str> = b["targets"].as_array().unwrap().iter().map(|t| t["upstream_model"].as_str().unwrap()).collect();
    assert_eq!(models, ["n1", "n2", "n3"]);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM route_targets WHERE route_id = $1")
        .bind(uid(&rid))
        .fetch_one(&a.pool)
        .await
        .unwrap();
    assert_eq!(n, 3, "old targets removed");

    // PATCH 冲突与失败的 PATCH 不改动数据
    let (s, _, _) = a.call("PATCH", &format!("/admin/routes/{rid}"), Some(json!({"public_model": "second"}))).await;
    assert_eq!(s, 409);
    let (s, b, _) = a
        .call("PATCH", &format!("/admin/routes/{rid}"), Some(json!({"targets": [target(ghost, "q")]})))
        .await;
    assert_eq!((s, b["error"]["field"].as_str()), (400, Some("targets[0].provider")));
    let (_, b, _) = a.call("GET", &format!("/admin/routes/{rid}"), None).await;
    assert_eq!((b["public_model"].as_str(), b["targets"].as_array().unwrap().len()), (Some("gpt-y"), 3));

    // 404
    let missing = "00000000-0000-0000-0000-000000000002";
    for m in ["GET", "DELETE"] {
        let (s, b, _) = a.call(m, &format!("/admin/routes/{missing}"), None).await;
        assert_eq!((s, b["error"]["type"].as_str()), (404, Some("not_found")), "{m}");
    }
    let (s, _, _) = a.call("PATCH", &format!("/admin/routes/{missing}"), Some(json!({"public_model": "x"}))).await;
    assert_eq!(s, 404);

    // 删除 Route 级联删除 Route_Target
    let (s, _, _) = a.call("DELETE", &format!("/admin/routes/{rid}"), None).await;
    assert_eq!(s, 204);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM route_targets WHERE route_id = $1")
        .bind(uid(&rid))
        .fetch_one(&a.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let (s, _, _) = a.call("GET", &format!("/admin/routes/{rid}"), None).await;
    assert_eq!(s, 404);
}

fn uid(s: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(s).unwrap()
}
