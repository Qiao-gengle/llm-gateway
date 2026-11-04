//! `/admin/routes` CRUD（需求 10）。
//!
//! 请求/响应形态：`{"public_model": "...", "targets": [{"provider": "<uuid>", "upstream_model": "...",
//! "priority": 0, "weight": 1, "default_max_tokens": null, "enabled": true}]}`。
//! `targets` 的数组顺序即 `position`。

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::AdminDb;
use crate::db::DbError;
use crate::db::routes::{self, NewTarget, RouteWithTargets};
use crate::http::errors::AdminError;

const MAX_TARGETS: usize = 32;

pub fn router(db: AdminDb) -> Router {
    Router::new()
        .route("/admin/routes", get(list).post(create))
        .route("/admin/routes/{id}", get(get_one).patch(patch).delete(remove))
        .with_state(db)
}

/// 已校验的请求字段；`None` 表示“未出现”。
#[derive(Debug, Default, PartialEq, Eq)]
struct Fields {
    public_model: Option<String>,
    targets: Option<Vec<NewTarget>>,
}

fn bad(field: &str, msg: &str) -> AdminError {
    AdminError::validation(field, format!("{field} {msg}"))
}

fn parse_body(body: &[u8]) -> Result<Map<String, Value>, AdminError> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(bad("body", "must be a JSON object")),
        Err(_) => Err(bad("body", "must be valid JSON")),
    }
}

fn model_name(v: &Value, field: &str) -> Result<String, AdminError> {
    match v {
        Value::String(s) => {
            let n = s.chars().count();
            if (1..=256).contains(&n) {
                Ok(s.clone())
            } else {
                Err(bad(field, "must be 1-256 characters"))
            }
        }
        _ => Err(bad(field, "must be a string")),
    }
}

fn int_in(v: &Value, field: &str, lo: i64, hi: i64) -> Result<i32, AdminError> {
    match v.as_i64().filter(|n| (lo..=hi).contains(n)) {
        Some(n) => Ok(n as i32),
        None => Err(bad(field, &format!("must be an integer between {lo} and {hi}"))),
    }
}

fn parse_target(i: usize, v: &Value) -> Result<NewTarget, AdminError> {
    let f = |name: &str| format!("targets[{i}].{name}");
    let Value::Object(m) = v else {
        return Err(bad(&format!("targets[{i}]"), "must be an object"));
    };
    let req = |name: &str| m.get(name).ok_or_else(|| bad(&f(name), "is required"));

    let provider = match req("provider")? {
        Value::String(s) => Uuid::parse_str(s).map_err(|_| bad(&f("provider"), "must be a provider id (UUID)"))?,
        _ => return Err(bad(&f("provider"), "must be a provider id (UUID)")),
    };
    let upstream_model = model_name(req("upstream_model")?, &f("upstream_model"))?;
    let priority = int_in(req("priority")?, &f("priority"), 0, 1000)?;
    let weight = int_in(req("weight")?, &f("weight"), 1, 10000)?;
    let default_max_tokens = match m.get("default_max_tokens") {
        None | Some(Value::Null) => None,
        Some(v) => Some(int_in(v, &f("default_max_tokens"), 1, i32::MAX as i64)?),
    };
    let enabled = match m.get("enabled") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(&f("enabled"), "must be a boolean")),
    };
    Ok(NewTarget { provider_id: provider, upstream_model, priority, weight, default_max_tokens, enabled })
}

fn parse_fields(m: &Map<String, Value>) -> Result<Fields, AdminError> {
    let public_model = m.get("public_model").map(|v| model_name(v, "public_model")).transpose()?;
    let targets = match m.get("targets") {
        None => None,
        Some(Value::Array(a)) => {
            if a.is_empty() || a.len() > MAX_TARGETS {
                return Err(bad("targets", &format!("must contain 1-{MAX_TARGETS} items")));
            }
            Some(a.iter().enumerate().map(|(i, v)| parse_target(i, v)).collect::<Result<Vec<_>, _>>()?)
        }
        Some(_) => return Err(bad("targets", "must be an array")),
    };
    Ok(Fields { public_model, targets })
}

fn view(r: &RouteWithTargets) -> Value {
    let targets: Vec<Value> = r
        .targets
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "provider": t.provider_id,
                "upstream_model": t.upstream_model,
                "priority": t.priority,
                "weight": t.weight,
                "default_max_tokens": t.default_max_tokens,
                "enabled": t.enabled,
            })
        })
        .collect();
    json!({
        "id": r.route.id,
        "public_model": r.route.public_model,
        "targets": targets,
        "created_at": r.route.created_at,
        "updated_at": r.route.updated_at,
    })
}

fn parse_id(id: &str) -> Result<Uuid, AdminError> {
    Uuid::parse_str(id).map_err(|_| not_found())
}

fn not_found() -> AdminError {
    AdminError::NotFound { message: "route not found".to_string() }
}

fn db_err(e: DbError) -> AdminError {
    if e.is_unique_violation() {
        AdminError::Conflict { message: "public_model already exists".to_string() }
    } else if e.is_foreign_key_violation() {
        // 事务内预检之后 Provider 被并发删除。
        bad("targets", "references a provider that does not exist")
    } else {
        tracing::warn!(error = %e, "admin routes: database error");
        AdminError::Unavailable
    }
}

async fn begin(db: &AdminDb) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AdminError> {
    db.pool.begin().await.map_err(|e| db_err(e.into()))
}

async fn commit(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), AdminError> {
    tx.commit().await.map_err(|e| db_err(e.into()))
}

/// 确认全部 Route_Target 引用的 Provider 存在；否则指出第一个缺失项的位置。
async fn check_providers(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    targets: &[NewTarget],
) -> Result<(), AdminError> {
    let ids: Vec<Uuid> = targets.iter().map(|t| t.provider_id).collect();
    let missing = routes::missing_providers(tx, &ids).await.map_err(db_err)?;
    if let Some(i) = targets.iter().position(|t| missing.contains(&t.provider_id)) {
        return Err(AdminError::validation(
            format!("targets[{i}].provider"),
            format!("targets[{i}].provider does not exist"),
        ));
    }
    Ok(())
}

async fn create(State(db): State<AdminDb>, body: Bytes) -> Result<Response, AdminError> {
    let f = parse_fields(&parse_body(&body)?)?;
    let public_model = f.public_model.ok_or_else(|| bad("public_model", "is required"))?;
    let targets = f.targets.ok_or_else(|| bad("targets", "is required"))?;

    let mut tx = begin(&db).await?;
    check_providers(&mut tx, &targets).await?;
    let r = routes::insert(&mut tx, &public_model, &targets).await.map_err(db_err)?;
    commit(tx).await?;
    db.refresh_registry().await;
    Ok((StatusCode::CREATED, Json(view(&r))).into_response())
}

async fn list(
    State(db): State<AdminDb>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AdminError> {
    let rows = routes::list(&db.pool, q.get("public_model").map(String::as_str))
        .await
        .map_err(db_err)?;
    Ok(Json(Value::Array(rows.iter().map(view).collect())))
}

async fn get_one(State(db): State<AdminDb>, Path(id): Path<String>) -> Result<Json<Value>, AdminError> {
    let id = parse_id(&id)?;
    let r = routes::get(&db.pool, id).await.map_err(db_err)?.ok_or_else(not_found)?;
    Ok(Json(view(&r)))
}

async fn patch(
    State(db): State<AdminDb>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, AdminError> {
    let id = parse_id(&id)?;
    let f = parse_fields(&parse_body(&body)?)?;

    let mut tx = begin(&db).await?;
    let cur = routes::get_for_update(&mut tx, id).await.map_err(db_err)?.ok_or_else(not_found)?;
    if let Some(ts) = &f.targets {
        check_providers(&mut tx, ts).await?;
    }
    let public_model = f.public_model.unwrap_or(cur.route.public_model);
    let r = routes::update(&mut tx, id, &public_model, f.targets.as_deref()).await.map_err(db_err)?;
    commit(tx).await?;
    db.refresh_registry().await;
    Ok(Json(view(&r)))
}

async fn remove(State(db): State<AdminDb>, Path(id): Path<String>) -> Result<StatusCode, AdminError> {
    let id = parse_id(&id)?;
    let mut tx = begin(&db).await?;
    if !routes::delete(&mut tx, id).await.map_err(db_err)? {
        return Err(not_found());
    }
    commit(tx).await?;
    db.refresh_registry().await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn field_of(e: AdminError) -> String {
        match e {
            AdminError::Validation { field: Some(f), .. } => f,
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    fn target() -> Value {
        json!({"provider": Uuid::nil(), "upstream_model": "gpt", "priority": 0, "weight": 1})
    }

    fn with(mut t: Value, k: &str, v: Value) -> Value {
        t[k] = v;
        t
    }

    #[test]
    fn route_field_validation_reports_field_name() {
        let t = target();
        let cases = [
            (json!({"public_model": ""}), "public_model"),
            (json!({"public_model": "a".repeat(257)}), "public_model"),
            (json!({"public_model": 1}), "public_model"),
            (json!({"public_model": null}), "public_model"),
            (json!({"targets": []}), "targets"),
            (json!({"targets": vec![t.clone(); 33]}), "targets"),
            (json!({"targets": "x"}), "targets"),
            (json!({"targets": [1]}), "targets[0]"),
            (json!({"targets": [with(t.clone(), "provider", json!("nope"))]}), "targets[0].provider"),
            (json!({"targets": [with(t.clone(), "upstream_model", json!(""))]}), "targets[0].upstream_model"),
            (json!({"targets": [with(t.clone(), "priority", json!(1001))]}), "targets[0].priority"),
            (json!({"targets": [with(t.clone(), "priority", json!(-1))]}), "targets[0].priority"),
            (json!({"targets": [with(t.clone(), "priority", json!(1.5))]}), "targets[0].priority"),
            (json!({"targets": [with(t.clone(), "weight", json!(0))]}), "targets[0].weight"),
            (json!({"targets": [with(t.clone(), "weight", json!(10001))]}), "targets[0].weight"),
            (json!({"targets": [with(t.clone(), "default_max_tokens", json!(0))]}), "targets[0].default_max_tokens"),
            (json!({"targets": [with(t.clone(), "enabled", json!(null))]}), "targets[0].enabled"),
            (json!({"targets": [t.clone(), with(t.clone(), "weight", json!("1"))]}), "targets[1].weight"),
            (json!({"targets": [json!({"provider": Uuid::nil()})]}), "targets[0].upstream_model"),
        ];
        for (v, field) in cases {
            assert_eq!(field_of(parse_fields(&obj(v.clone())).unwrap_err()), field, "{v}");
        }
    }

    #[test]
    fn valid_fields_parse_with_defaults_and_boundaries() {
        assert_eq!(parse_fields(&obj(json!({}))).unwrap(), Fields::default());
        let f = parse_fields(&obj(json!({
            "public_model": "m",
            "targets": [target(), with(with(target(), "priority", json!(1000)), "weight", json!(10000))]
        })))
        .unwrap();
        let ts = f.targets.unwrap();
        assert_eq!(ts.len(), 2);
        assert!(ts[0].enabled && ts[0].default_max_tokens.is_none());
        assert_eq!((ts[1].priority, ts[1].weight), (1000, 10000));
        let f = parse_fields(&obj(json!({"targets": [with(target(), "default_max_tokens", json!(4096))]}))).unwrap();
        assert_eq!(f.targets.unwrap()[0].default_max_tokens, Some(4096));
        let f = parse_fields(&obj(json!({"targets": vec![target(); 32]}))).unwrap();
        assert_eq!(f.targets.unwrap().len(), 32);
    }

    #[test]
    fn body_must_be_json_object() {
        assert_eq!(field_of(parse_body(b"{bad").unwrap_err()), "body");
        assert_eq!(field_of(parse_body(b"[]").unwrap_err()), "body");
    }

    #[test]
    fn view_lists_targets_in_given_order_with_provider_field() {
        use crate::db::routes::{RouteRow, TargetRow};
        let now = chrono::Utc::now();
        let mk = |pos: i32| TargetRow {
            id: Uuid::nil(),
            route_id: Uuid::nil(),
            position: pos,
            provider_id: Uuid::nil(),
            upstream_model: format!("m{pos}"),
            priority: 1,
            weight: 2,
            default_max_tokens: None,
            enabled: true,
        };
        let v = view(&RouteWithTargets {
            route: RouteRow { id: Uuid::nil(), public_model: "pm".into(), created_at: now, updated_at: now },
            targets: vec![mk(0), mk(1)],
        });
        assert_eq!(v["targets"][0]["upstream_model"], "m0");
        assert_eq!(v["targets"][1]["upstream_model"], "m1");
        assert!(v["targets"][0].get("provider").is_some());
        assert!(v["targets"][0]["default_max_tokens"].is_null());
    }

    /// 无数据库的 HTTP 层测试：连接池指向不可达地址。
    async fn call(method: &str, path: &str, auth: bool, body: &str) -> (u16, Value) {
        use crate::clock::OsRandom;
        use crate::http::admin::{AdminState, admin_router};
        use secrecy::SecretString;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect_lazy("postgres://u:p@127.0.0.1:1/db")
            .unwrap();
        let secrets = std::sync::Arc::new(crate::secret::SecretStore::new(&[1u8; 32], std::sync::Arc::new(OsRandom)));
        let app = admin_router(AdminState::new(&SecretString::from("tok")).with_db(pool, secrets));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut req = reqwest::Client::new()
            .request(method.parse().unwrap(), format!("http://{addr}{path}"))
            .body(body.to_string());
        if auth {
            req = req.header("authorization", "Bearer tok");
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn http_auth_precedes_validation_and_db_down_is_503() {
        let (s, _) = call("POST", "/admin/routes", false, "{bad").await;
        assert_eq!(s, 401);
        let (s, b) = call("POST", "/admin/routes", true, "{bad").await;
        assert_eq!((s, b["error"]["field"].as_str()), (400, Some("body")));
        let (s, b) = call("POST", "/admin/routes", true, r#"{"targets":[]}"#).await;
        assert_eq!((s, b["error"]["field"].as_str()), (400, Some("targets")));
        let (s, b) = call("POST", "/admin/routes", true, r#"{"targets":[]}"#).await;
        assert_eq!((s, b["error"]["field"].as_str()), (400, Some("targets")));
        let (s, _) = call("GET", "/admin/routes", true, "").await;
        assert_eq!(s, 503);
        let (s, _) = call("GET", "/admin/routes?public_model=x", true, "").await;
        assert_eq!(s, 503);
        let (s, _) = call("GET", "/admin/routes/not-a-uuid", true, "").await;
        assert_eq!(s, 404);
        let (s, _) = call("DELETE", &format!("/admin/routes/{}", Uuid::nil()), true, "").await;
        assert_eq!(s, 503);
    }
}
