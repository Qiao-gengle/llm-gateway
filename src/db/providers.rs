//! `providers` 表的数据访问（需求 9）。写操作由调用方在事务中完成。

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::DbError;
use crate::secret::Sealed;

/// 一行 Provider；不含 `api_key_nonce` / `api_key_ciphertext`，这两列永远不被读出。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ProviderRow {
    pub id: Uuid,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub connect_timeout_s: i32,
    pub ttfb_timeout_s: i32,
    pub total_timeout_s: i32,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// INSERT 的完整字段。
#[derive(Debug)]
pub struct NewProvider {
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub sealed_key: Sealed,
    pub connect_timeout_s: i32,
    pub ttfb_timeout_s: i32,
    pub total_timeout_s: i32,
    pub enabled: bool,
}

/// PATCH 合并后的字段；`sealed_key` 为 `None` 表示保留原 Upstream_Key。
#[derive(Debug)]
pub struct ProviderUpdate {
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub sealed_key: Option<Sealed>,
    pub connect_timeout_s: i32,
    pub ttfb_timeout_s: i32,
    pub total_timeout_s: i32,
    pub enabled: bool,
}

const COLUMNS: &str = "id, name, protocol, base_url, connect_timeout_s, ttfb_timeout_s, \
                       total_timeout_s, enabled, created_at, updated_at";

pub async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    p: &NewProvider,
) -> Result<ProviderRow, DbError> {
    let sql = format!(
        "INSERT INTO providers (name, protocol, base_url, api_key_nonce, api_key_ciphertext, \
         connect_timeout_s, ttfb_timeout_s, total_timeout_s, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {COLUMNS}"
    );
    sqlx::query_as::<_, ProviderRow>(&sql)
        .bind(&p.name)
        .bind(&p.protocol)
        .bind(&p.base_url)
        .bind(p.sealed_key.nonce.as_slice())
        .bind(p.sealed_key.ciphertext.as_slice())
        .bind(p.connect_timeout_s)
        .bind(p.ttfb_timeout_s)
        .bind(p.total_timeout_s)
        .bind(p.enabled)
        .fetch_one(&mut **tx)
        .await
        .map_err(DbError::from)
}

pub async fn list(pool: &PgPool) -> Result<Vec<ProviderRow>, DbError> {
    let sql = format!("SELECT {COLUMNS} FROM providers ORDER BY created_at, id");
    sqlx::query_as::<_, ProviderRow>(&sql)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<ProviderRow>, DbError> {
    let sql = format!("SELECT {COLUMNS} FROM providers WHERE id = $1");
    sqlx::query_as::<_, ProviderRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(DbError::from)
}

/// 事务内加行锁读取，供 PATCH 合并字段。
pub async fn get_for_update(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<ProviderRow>, DbError> {
    let sql = format!("SELECT {COLUMNS} FROM providers WHERE id = $1 FOR UPDATE");
    sqlx::query_as::<_, ProviderRow>(&sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(DbError::from)
}

pub async fn update(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    u: &ProviderUpdate,
) -> Result<ProviderRow, DbError> {
    let (nonce, ct): (Option<&[u8]>, Option<&[u8]>) = match &u.sealed_key {
        Some(s) => (Some(s.nonce.as_slice()), Some(s.ciphertext.as_slice())),
        None => (None, None),
    };
    let sql = format!(
        "UPDATE providers SET name = $2, protocol = $3, base_url = $4, \
         api_key_nonce = COALESCE($5, api_key_nonce), \
         api_key_ciphertext = COALESCE($6, api_key_ciphertext), \
         connect_timeout_s = $7, ttfb_timeout_s = $8, total_timeout_s = $9, enabled = $10 \
         WHERE id = $1 RETURNING {COLUMNS}"
    );
    sqlx::query_as::<_, ProviderRow>(&sql)
        .bind(id)
        .bind(&u.name)
        .bind(&u.protocol)
        .bind(&u.base_url)
        .bind(nonce)
        .bind(ct)
        .bind(u.connect_timeout_s)
        .bind(u.ttfb_timeout_s)
        .bind(u.total_timeout_s)
        .bind(u.enabled)
        .fetch_one(&mut **tx)
        .await
        .map_err(DbError::from)
}

/// 返回是否删除了一行。被 `route_targets` 引用时数据库返回 23503。
pub async fn delete(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<bool, DbError> {
    let res = sqlx::query("DELETE FROM providers WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(DbError::from)?;
    Ok(res.rows_affected() > 0)
}
