//! `routes` 与 `route_targets` 表的数据访问（需求 10）。写操作由调用方在事务中完成。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use super::DbError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RouteRow {
    pub id: Uuid,
    pub public_model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TargetRow {
    pub id: Uuid,
    pub route_id: Uuid,
    pub position: i32,
    pub provider_id: Uuid,
    pub upstream_model: String,
    pub priority: i32,
    pub weight: i32,
    pub default_max_tokens: Option<i32>,
    pub enabled: bool,
}

/// 一个 Route 及其按 `position` 排序的全部 Route_Target。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteWithTargets {
    pub route: RouteRow,
    pub targets: Vec<TargetRow>,
}

/// 写入用的 Route_Target；`position` 由其在列表中的下标决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTarget {
    pub provider_id: Uuid,
    pub upstream_model: String,
    pub priority: i32,
    pub weight: i32,
    pub default_max_tokens: Option<i32>,
    pub enabled: bool,
}

const ROUTE_COLUMNS: &str = "id, public_model, created_at, updated_at";
const TARGET_COLUMNS: &str = "id, route_id, position, provider_id, upstream_model, priority, \
                              weight, default_max_tokens, enabled";

/// 返回 `ids` 中在 `providers` 表里不存在的 id。
pub async fn missing_providers(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[Uuid],
) -> Result<Vec<Uuid>, DbError> {
    let found: Vec<(Uuid,)> = sqlx::query_as("SELECT id FROM providers WHERE id = ANY($1)")
        .bind(ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(DbError::from)?;
    let found: std::collections::HashSet<Uuid> = found.into_iter().map(|r| r.0).collect();
    Ok(ids.iter().copied().filter(|i| !found.contains(i)).collect())
}

async fn insert_targets(
    tx: &mut Transaction<'_, Postgres>,
    route_id: Uuid,
    targets: &[NewTarget],
) -> Result<(), DbError> {
    for (i, t) in targets.iter().enumerate() {
        sqlx::query(
            "INSERT INTO route_targets (route_id, position, provider_id, upstream_model, \
             priority, weight, default_max_tokens, enabled) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(route_id)
        .bind(i as i32)
        .bind(t.provider_id)
        .bind(&t.upstream_model)
        .bind(t.priority)
        .bind(t.weight)
        .bind(t.default_max_tokens)
        .bind(t.enabled)
        .execute(&mut **tx)
        .await
        .map_err(DbError::from)?;
    }
    Ok(())
}

async fn targets_of(
    tx: &mut Transaction<'_, Postgres>,
    route_id: Uuid,
) -> Result<Vec<TargetRow>, DbError> {
    let sql = format!("SELECT {TARGET_COLUMNS} FROM route_targets WHERE route_id = $1 ORDER BY position");
    sqlx::query_as::<_, TargetRow>(&sql)
        .bind(route_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(DbError::from)
}

pub async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    public_model: &str,
    targets: &[NewTarget],
) -> Result<RouteWithTargets, DbError> {
    let sql = format!("INSERT INTO routes (public_model) VALUES ($1) RETURNING {ROUTE_COLUMNS}");
    let route = sqlx::query_as::<_, RouteRow>(&sql)
        .bind(public_model)
        .fetch_one(&mut **tx)
        .await
        .map_err(DbError::from)?;
    insert_targets(tx, route.id, targets).await?;
    let targets = targets_of(tx, route.id).await?;
    Ok(RouteWithTargets { route, targets })
}

/// 事务内加行锁读取 Route 及其 Route_Target，供 PATCH 合并字段。
pub async fn get_for_update(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Option<RouteWithTargets>, DbError> {
    let sql = format!("SELECT {ROUTE_COLUMNS} FROM routes WHERE id = $1 FOR UPDATE");
    let Some(route) = sqlx::query_as::<_, RouteRow>(&sql)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(DbError::from)?
    else {
        return Ok(None);
    };
    let targets = targets_of(tx, id).await?;
    Ok(Some(RouteWithTargets { route, targets }))
}

/// 更新 `public_model`；`new_targets` 为 `Some` 时整体替换 Route_Target 列表。
/// 无论是否修改字段都会触发 `updated_at`，使 Registry 指纹变化。
pub async fn update(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    public_model: &str,
    new_targets: Option<&[NewTarget]>,
) -> Result<RouteWithTargets, DbError> {
    let sql = format!("UPDATE routes SET public_model = $2 WHERE id = $1 RETURNING {ROUTE_COLUMNS}");
    let route = sqlx::query_as::<_, RouteRow>(&sql)
        .bind(id)
        .bind(public_model)
        .fetch_one(&mut **tx)
        .await
        .map_err(DbError::from)?;
    if let Some(ts) = new_targets {
        sqlx::query("DELETE FROM route_targets WHERE route_id = $1")
            .bind(id)
            .execute(&mut **tx)
            .await
            .map_err(DbError::from)?;
        insert_targets(tx, id, ts).await?;
    }
    let targets = targets_of(tx, id).await?;
    Ok(RouteWithTargets { route, targets })
}

/// 返回是否删除了一行；`route_targets` 经 `ON DELETE CASCADE` 一并删除。
pub async fn delete(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<bool, DbError> {
    let res = sqlx::query("DELETE FROM routes WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(DbError::from)?;
    Ok(res.rows_affected() > 0)
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<RouteWithTargets>, DbError> {
    let sql = format!("SELECT {ROUTE_COLUMNS} FROM routes WHERE id = $1");
    let Some(route) = sqlx::query_as::<_, RouteRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(DbError::from)?
    else {
        return Ok(None);
    };
    let sql = format!("SELECT {TARGET_COLUMNS} FROM route_targets WHERE route_id = $1 ORDER BY position");
    let targets = sqlx::query_as::<_, TargetRow>(&sql)
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?;
    Ok(Some(RouteWithTargets { route, targets }))
}

/// 列出全部 Route（可按 `public_model` 精确过滤，区分大小写）。
pub async fn list(pool: &PgPool, public_model: Option<&str>) -> Result<Vec<RouteWithTargets>, DbError> {
    let sql = format!(
        "SELECT {ROUTE_COLUMNS} FROM routes WHERE ($1::text IS NULL OR public_model = $1) \
         ORDER BY created_at, id"
    );
    let routes = sqlx::query_as::<_, RouteRow>(&sql)
        .bind(public_model)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?;
    let ids: Vec<Uuid> = routes.iter().map(|r| r.id).collect();
    let sql = format!(
        "SELECT {TARGET_COLUMNS} FROM route_targets WHERE route_id = ANY($1) ORDER BY route_id, position"
    );
    let all = sqlx::query_as::<_, TargetRow>(&sql)
        .bind(&ids)
        .fetch_all(pool)
        .await
        .map_err(DbError::from)?;
    let mut by_route: HashMap<Uuid, Vec<TargetRow>> = HashMap::new();
    for t in all {
        by_route.entry(t.route_id).or_default().push(t);
    }
    Ok(routes
        .into_iter()
        .map(|route| {
            let targets = by_route.remove(&route.id).unwrap_or_default();
            RouteWithTargets { route, targets }
        })
        .collect())
}
