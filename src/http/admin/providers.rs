//! `/admin/providers` CRUD（需求 9）。响应从不包含 Upstream_Key 的明文、密文与 nonce。

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::AdminDb;
use crate::db::providers::{self, NewProvider, ProviderRow, ProviderUpdate};
use crate::db::DbError;
use crate::http::errors::AdminError;

const DEFAULT_CONNECT_S: i32 = 5;
const DEFAULT_TTFB_S: i32 = 60;
const DEFAULT_TOTAL_S: i32 = 600;

pub fn router(db: AdminDb) -> Router {
    Router::new()
        .route("/admin/providers", get(list).post(create))
        .route(
            "/admin/providers/{id}",
            get(get_one).patch(patch).delete(remove),
        )
        .with_state(db)
}

/// 已逐字段校验、尚未应用默认值的请求字段；`None` 表示“未出现”。
#[derive(Debug, Default, PartialEq, Eq)]
struct Fields {
    name: Option<String>,
    protocol: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    connect_timeout: Option<i32>,
    ttfb_timeout: Option<i32>,
    total_timeout: Option<i32>,
    enabled: Option<bool>,
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

fn str_field(m: &Map<String, Value>, key: &str, min: usize, max: usize) -> Result<Option<String>, AdminError> {
    match m.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => {
            let n = s.chars().count();
            if n < min || n > max {
                Err(bad(key, &format!("must be {min}-{max} characters")))
            } else {
                Ok(Some(s.clone()))
            }
        }
        Some(_) => Err(bad(key, "must be a string")),
    }
}

fn timeout_field(m: &Map<String, Value>, key: &str) -> Result<Option<i32>, AdminError> {
    match m.get(key) {
        None => Ok(None),
        Some(v) => match v.as_i64().filter(|n| *n > 0).and_then(|n| i32::try_from(n).ok()) {
            Some(n) => Ok(Some(n)),
            None => Err(bad(key, "must be a positive integer number of seconds")),
        },
    }
}

fn parse_fields(m: &Map<String, Value>) -> Result<Fields, AdminError> {
    let name = str_field(m, "name", 1, 64)?;
    let protocol = match m.get("protocol") {
        None => None,
        Some(Value::String(s)) if s == "openai" || s == "anthropic" => Some(s.clone()),
        Some(_) => return Err(bad("protocol", "must be \"openai\" or \"anthropic\"")),
    };
    let base_url = match m.get("base_url") {
        None => None,
        Some(Value::String(s)) => match reqwest::Url::parse(s) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.has_host() => Some(s.clone()),
            _ => return Err(bad("base_url", "must be an absolute http or https URL")),
        },
        Some(_) => return Err(bad("base_url", "must be a string")),
    };
    let api_key = str_field(m, "api_key", 1, 4096)?;
    let enabled = match m.get("enabled") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(bad("enabled", "must be a boolean")),
    };
    Ok(Fields {
        name,
        protocol,
        base_url,
        api_key,
        connect_timeout: timeout_field(m, "connect_timeout")?,
        ttfb_timeout: timeout_field(m, "ttfb_timeout")?,
        total_timeout: timeout_field(m, "total_timeout")?,
        enabled,
    })
}

fn check_timeouts(connect: i32, ttfb: i32, total: i32) -> Result<(), AdminError> {
    if connect > total {
        return Err(bad("connect_timeout", "must not exceed total_timeout"));
    }
    if ttfb > total {
        return Err(bad("ttfb_timeout", "must not exceed total_timeout"));
    }
    Ok(())
}

fn required<T>(v: Option<T>, field: &str) -> Result<T, AdminError> {
    v.ok_or_else(|| bad(field, "is required"))
}

fn view(p: &ProviderRow) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "protocol": p.protocol,
        "base_url": p.base_url,
        "connect_timeout": p.connect_timeout_s,
        "ttfb_timeout": p.ttfb_timeout_s,
        "total_timeout": p.total_timeout_s,
        "enabled": p.enabled,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

fn parse_id(id: &str) -> Result<Uuid, AdminError> {
    Uuid::parse_str(id).map_err(|_| not_found())
}

fn not_found() -> AdminError {
    AdminError::NotFound { message: "provider not found".to_string() }
}

/// 将数据库错误映射为 Admin 错误；`name` 唯一冲突与外键引用给出明确信息。
fn db_err(e: DbError) -> AdminError {
    if e.is_unique_violation() {
        AdminError::Conflict { message: "provider name already exists".to_string() }
    } else if e.is_foreign_key_violation() {
        AdminError::Conflict {
            message: "provider is referenced by route targets".to_string(),
        }
    } else {
        tracing::warn!(error = %e, "admin providers: database error");
        AdminError::Unavailable
    }
}

async fn begin(db: &AdminDb) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AdminError> {
    db.pool.begin().await.map_err(|e| db_err(e.into()))
}

async fn commit(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), AdminError> {
    tx.commit().await.map_err(|e| db_err(e.into()))
}

async fn create(State(db): State<AdminDb>, body: Bytes) -> Result<Response, AdminError> {
    let f = parse_fields(&parse_body(&body)?)?;
    let name = required(f.name, "name")?;
    let protocol = required(f.protocol, "protocol")?;
    let base_url = required(f.base_url, "base_url")?;
    let api_key = required(f.api_key, "api_key")?;
    let connect = f.connect_timeout.unwrap_or(DEFAULT_CONNECT_S);
    let ttfb = f.ttfb_timeout.unwrap_or(DEFAULT_TTFB_S);
    let total = f.total_timeout.unwrap_or(DEFAULT_TOTAL_S);
    check_timeouts(connect, ttfb, total)?;

    let new = NewProvider {
        name,
        protocol,
        base_url,
        sealed_key: db.secrets.seal(api_key.as_bytes()),
        connect_timeout_s: connect,
        ttfb_timeout_s: ttfb,
        total_timeout_s: total,
        enabled: f.enabled.unwrap_or(true),
    };
    let mut tx = begin(&db).await?;
    let row = providers::insert(&mut tx, &new).await.map_err(db_err)?;
    commit(tx).await?;
    Ok((StatusCode::CREATED, Json(view(&row))).into_response())
}

async fn list(State(db): State<AdminDb>) -> Result<Json<Value>, AdminError> {
    let rows = providers::list(&db.pool).await.map_err(db_err)?;
    Ok(Json(Value::Array(rows.iter().map(view).collect())))
}

async fn get_one(State(db): State<AdminDb>, Path(id): Path<String>) -> Result<Json<Value>, AdminError> {
    let id = parse_id(&id)?;
    let row = providers::get(&db.pool, id).await.map_err(db_err)?.ok_or_else(not_found)?;
    Ok(Json(view(&row)))
}

async fn patch(
    State(db): State<AdminDb>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, AdminError> {
    let id = parse_id(&id)?;
    let f = parse_fields(&parse_body(&body)?)?;

    let mut tx = begin(&db).await?;
    let cur = providers::get_for_update(&mut tx, id).await.map_err(db_err)?.ok_or_else(not_found)?;
    let upd = ProviderUpdate {
        name: f.name.unwrap_or(cur.name),
        protocol: f.protocol.unwrap_or(cur.protocol),
        base_url: f.base_url.unwrap_or(cur.base_url),
        sealed_key: f.api_key.map(|k| db.secrets.seal(k.as_bytes())),
        connect_timeout_s: f.connect_timeout.unwrap_or(cur.connect_timeout_s),
        ttfb_timeout_s: f.ttfb_timeout.unwrap_or(cur.ttfb_timeout_s),
        total_timeout_s: f.total_timeout.unwrap_or(cur.total_timeout_s),
        enabled: f.enabled.unwrap_or(cur.enabled),
    };
    check_timeouts(upd.connect_timeout_s, upd.ttfb_timeout_s, upd.total_timeout_s)?;
    let row = providers::update(&mut tx, id, &upd).await.map_err(db_err)?;
    commit(tx).await?;
    Ok(Json(view(&row)))
}

async fn remove(State(db): State<AdminDb>, Path(id): Path<String>) -> Result<StatusCode, AdminError> {
    let id = parse_id(&id)?;
    let mut tx = begin(&db).await?;
    if !providers::delete(&mut tx, id).await.map_err(db_err)? {
        return Err(not_found());
    }
    commit(tx).await?;
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

    #[test]
    fn field_validation_reports_field_name() {
        let cases = [
            (json!({"name": ""}), "name"),
            (json!({"name": "a".repeat(65)}), "name"),
            (json!({"protocol": "gemini"}), "protocol"),
            (json!({"base_url": "ftp://x"}), "base_url"),
            (json!({"base_url": "/relative"}), "base_url"),
            (json!({"api_key": ""}), "api_key"),
            (json!({"api_key": "k".repeat(4097)}), "api_key"),
            (json!({"connect_timeout": 0}), "connect_timeout"),
            (json!({"ttfb_timeout": 1.5}), "ttfb_timeout"),
            (json!({"total_timeout": "5"}), "total_timeout"),
            (json!({"enabled": null}), "enabled"),
        ];
        for (v, field) in cases {
            assert_eq!(field_of(parse_fields(&obj(v.clone())).unwrap_err()), field, "{v}");
        }
    }

    #[test]
    fn absent_fields_stay_none_and_valid_values_parse() {
        assert_eq!(parse_fields(&obj(json!({}))).unwrap(), Fields::default());
        let f = parse_fields(&obj(json!({
            "name": "p", "protocol": "anthropic", "base_url": "https://api.example.com/v1",
            "api_key": "k", "connect_timeout": 1, "enabled": false
        })))
        .unwrap();
        assert_eq!(f.name.as_deref(), Some("p"));
        assert_eq!(f.connect_timeout, Some(1));
        assert_eq!(f.enabled, Some(false));
    }

    #[test]
    fn timeout_ordering() {
        assert!(check_timeouts(5, 60, 600).is_ok());
        assert!(check_timeouts(600, 600, 600).is_ok());
        assert_eq!(field_of(check_timeouts(7, 1, 6).unwrap_err()), "connect_timeout");
        assert_eq!(field_of(check_timeouts(1, 7, 6).unwrap_err()), "ttfb_timeout");
    }

    #[test]
    fn body_must_be_json_object() {
        assert_eq!(field_of(parse_body(b"{bad").unwrap_err()), "body");
        assert_eq!(field_of(parse_body(b"[]").unwrap_err()), "body");
        assert!(parse_body(b"{}").is_ok());
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
    async fn http_auth_precedes_body_validation_and_db_down_is_503() {
        let (s, _) = call("POST", "/admin/providers", false, "{bad").await;
        assert_eq!(s, 401);
        let (s, b) = call("POST", "/admin/providers", true, "{bad").await;
        assert_eq!((s, b["error"]["field"].as_str()), (400, Some("body")));
        let (s, b) = call("POST", "/admin/providers", true, r#"{"name":"a"}"#).await;
        assert_eq!((s, b["error"]["field"].as_str()), (400, Some("protocol")));
        let (s, _) = call("GET", "/admin/providers", true, "").await;
        assert_eq!(s, 503);
        let (s, _) = call("GET", "/admin/providers/not-a-uuid", true, "").await;
        assert_eq!(s, 404);
        let (s, _) = call("GET", &format!("/admin/providers/{}", Uuid::nil()), true, "").await;
        assert_eq!(s, 503);
        // 其他 /admin 路径仍走占位 404，且路由表无冲突
        let (s, _) = call("GET", "/admin/unknown", true, "").await;
        assert_eq!(s, 404);
    }

    #[test]
    fn view_has_no_key_material() {
        let now = chrono::Utc::now();
        let v = view(&ProviderRow {
            id: Uuid::nil(),
            name: "n".into(),
            protocol: "openai".into(),
            base_url: "http://h".into(),
            connect_timeout_s: 5,
            ttfb_timeout_s: 60,
            total_timeout_s: 600,
            enabled: true,
            created_at: now,
            updated_at: now,
        });
        let s = v.to_string();
        assert!(!s.contains("api_key") && !s.contains("nonce") && !s.contains("ciphertext"));
    }
}
