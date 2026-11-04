//! Registry：Provider / Route 的内存快照（见 design.md“Registry”，需求 13–14）。
//!
//! 快照以 `ArcSwap<Snapshot>` 保存。请求持有的 `Arc<Snapshot>` 在替换后保持不变。
//! 全量加载失败时保留旧快照。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Url;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::db::DbError;
use crate::secret::{DecryptError, Sealed, SecretStore};

pub type ProviderId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamProtocol {
    OpenAi,
    Anthropic,
}

impl UpstreamProtocol {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub ttfb: Duration,
    pub total: Duration,
}

#[derive(Clone)]
pub struct ProviderEntry {
    pub id: ProviderId,
    pub name: String,
    pub protocol: UpstreamProtocol,
    pub base_url: Url,
    /// `None` 表示 Upstream_Key 解密失败，该 Provider 不可用（需求 13 第 5 条）。
    pub api_key: Option<SecretString>,
    pub timeouts: Timeouts,
    pub enabled: bool,
}

impl std::fmt::Debug for ProviderEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEntry")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("protocol", &self.protocol)
            .field("api_key_available", &self.api_key.is_some())
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl ProviderEntry {
    /// Provider 已启用且 Upstream_Key 可用。
    pub fn is_available(&self) -> bool {
        self.enabled && self.api_key.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub id: Uuid,
    pub provider_id: ProviderId,
    pub upstream_model: String,
    pub priority: u16,
    pub weight: u32,
    pub default_max_tokens: Option<u32>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteEntry {
    pub id: Uuid,
    pub public_model: String,
    /// 按 Admin 提交顺序（`position`）排列。
    pub targets: Vec<RouteTarget>,
}

/// 三张表的行数与最大 `updated_at`，用于检测外部修改。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub providers_count: i64,
    pub providers_max_updated: Option<DateTime<Utc>>,
    pub routes_count: i64,
    pub routes_max_updated: Option<DateTime<Utc>>,
    pub targets_count: i64,
    pub targets_max_updated: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub providers: HashMap<ProviderId, ProviderEntry>,
    /// 键为 public_model
    pub routes_by_model: HashMap<String, Arc<RouteEntry>>,
    pub fingerprint: Fingerprint,
}

impl Snapshot {
    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
            routes_by_model: HashMap::new(),
            fingerprint: Fingerprint::default(),
        }
    }

    /// `/v1/models` 的可见模型（需求 4.2–4.5）：至少有一个 enabled Route_Target 的 Public_Model，
    /// 与 `allowed_models` 求交（`None` 表示不限制）；按字典序严格升序，无重复。
    pub fn visible_models(&self, allowed_models: Option<&[String]>) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .routes_by_model
            .iter()
            .filter(|(_, r)| r.targets.iter().any(|t| t.enabled))
            .map(|(m, _)| m.as_str())
            .filter(|m| allowed_models.is_none_or(|a| a.iter().any(|x| x == m)))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

// ---------------------------------------------------------------------------
// 存储接缝
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RawProvider {
    pub id: Uuid,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub api_key_nonce: Vec<u8>,
    pub api_key_ciphertext: Vec<u8>,
    pub connect_timeout_s: i32,
    pub ttfb_timeout_s: i32,
    pub total_timeout_s: i32,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RawRoute {
    pub id: Uuid,
    pub public_model: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RawTarget {
    pub id: Uuid,
    pub route_id: Uuid,
    pub position: i32,
    pub provider_id: Uuid,
    pub upstream_model: String,
    pub priority: i32,
    pub weight: i32,
    pub default_max_tokens: Option<i32>,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
}

/// 同一事务内读出的三张表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawData {
    pub providers: Vec<RawProvider>,
    pub routes: Vec<RawRoute>,
    pub targets: Vec<RawTarget>,
}

impl RawData {
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            providers_count: self.providers.len() as i64,
            providers_max_updated: self.providers.iter().map(|r| r.updated_at).max(),
            routes_count: self.routes.len() as i64,
            routes_max_updated: self.routes.iter().map(|r| r.updated_at).max(),
            targets_count: self.targets.len() as i64,
            targets_max_updated: self.targets.iter().map(|r| r.updated_at).max(),
        }
    }
}

/// Registry 错误；`Display` 只含 `DbError` 的 SQLSTATE 与类别。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("registry store failed: {0}")]
    Store(#[from] DbError),
}

#[async_trait]
pub trait RegistryStore: Send + Sync {
    /// 在一致性读取（`REPEATABLE READ` 只读事务）中读取三张表。
    async fn load_all(&self) -> Result<RawData, RegistryError>;
    /// 读取当前指纹，用于轮询。
    async fn fingerprint(&self) -> Result<Fingerprint, RegistryError>;
}

pub struct PgRegistryStore {
    pool: sqlx::PgPool,
}

impl PgRegistryStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl RegistryStore for PgRegistryStore {
    async fn load_all(&self) -> Result<RawData, RegistryError> {
        let mut tx = self.pool.begin().await.map_err(DbError::from)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        let providers = sqlx::query_as::<_, RawProvider>(
            "SELECT id, name, protocol, base_url, api_key_nonce, api_key_ciphertext, \
             connect_timeout_s, ttfb_timeout_s, total_timeout_s, enabled, updated_at \
             FROM providers ORDER BY created_at, id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        let routes = sqlx::query_as::<_, RawRoute>(
            "SELECT id, public_model, updated_at FROM routes ORDER BY created_at, id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        let targets = sqlx::query_as::<_, RawTarget>(
            "SELECT id, route_id, position, provider_id, upstream_model, priority, weight, \
             default_max_tokens, enabled, updated_at FROM route_targets ORDER BY route_id, position",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(RawData { providers, routes, targets })
    }

    async fn fingerprint(&self) -> Result<Fingerprint, RegistryError> {
        type Row = (i64, Option<DateTime<Utc>>, i64, Option<DateTime<Utc>>, i64, Option<DateTime<Utc>>);
        let r: Row = sqlx::query_as(
            "SELECT (SELECT count(*) FROM providers), (SELECT max(updated_at) FROM providers), \
                    (SELECT count(*) FROM routes), (SELECT max(updated_at) FROM routes), \
                    (SELECT count(*) FROM route_targets), (SELECT max(updated_at) FROM route_targets)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from)?;
        Ok(Fingerprint {
            providers_count: r.0,
            providers_max_updated: r.1,
            routes_count: r.2,
            routes_max_updated: r.3,
            targets_count: r.4,
            targets_max_updated: r.5,
        })
    }
}

// ---------------------------------------------------------------------------
// 快照构建
// ---------------------------------------------------------------------------

fn open_key(secrets: &SecretStore, p: &RawProvider) -> Result<SecretString, DecryptError> {
    let nonce: [u8; 12] = p.api_key_nonce.as_slice().try_into().map_err(|_| DecryptError::BadNonceLength)?;
    secrets.open(&Sealed { nonce, ciphertext: p.api_key_ciphertext.clone() })
}

fn secs(v: i32) -> Duration {
    Duration::from_secs(u64::try_from(v).unwrap_or(0))
}

/// 把原始行转换为快照。
///
/// - 解密失败的 Provider 保留在快照中，`api_key` 为 `None`；错误日志只含 Provider id、name 与错误类别。
/// - 协议或 `base_url` 无法解析的 Provider 不进入快照（Admin_API 已校验，此情形只在数据被外部改坏时出现），
///   引用它的 Route_Target 因此找不到 Provider 而不可用。
pub fn build_snapshot(raw: &RawData, secrets: &SecretStore) -> Snapshot {
    let mut providers = HashMap::with_capacity(raw.providers.len());
    for p in &raw.providers {
        let (Some(protocol), Ok(base_url)) = (UpstreamProtocol::parse(&p.protocol), Url::parse(&p.base_url)) else {
            tracing::error!(provider_id = %p.id, provider_name = %p.name, "provider has invalid protocol or base_url; skipped");
            continue;
        };
        let api_key = match open_key(secrets, p) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::error!(provider_id = %p.id, provider_name = %p.name, error = %e, "failed to decrypt upstream key; provider unavailable");
                None
            }
        };
        providers.insert(
            p.id,
            ProviderEntry {
                id: p.id,
                name: p.name.clone(),
                protocol,
                base_url,
                api_key,
                timeouts: Timeouts {
                    connect: secs(p.connect_timeout_s),
                    ttfb: secs(p.ttfb_timeout_s),
                    total: secs(p.total_timeout_s),
                },
                enabled: p.enabled,
            },
        );
    }

    let mut targets_by_route: HashMap<Uuid, Vec<&RawTarget>> = HashMap::new();
    for t in &raw.targets {
        targets_by_route.entry(t.route_id).or_default().push(t);
    }
    let mut routes_by_model = HashMap::with_capacity(raw.routes.len());
    for r in &raw.routes {
        let mut ts = targets_by_route.remove(&r.id).unwrap_or_default();
        ts.sort_by_key(|t| t.position);
        let targets = ts
            .into_iter()
            .map(|t| RouteTarget {
                id: t.id,
                provider_id: t.provider_id,
                upstream_model: t.upstream_model.clone(),
                priority: t.priority.clamp(0, i32::from(u16::MAX)) as u16,
                weight: u32::try_from(t.weight).unwrap_or(0),
                default_max_tokens: t.default_max_tokens.and_then(|v| u32::try_from(v).ok()),
                enabled: t.enabled,
            })
            .collect();
        routes_by_model.insert(
            r.public_model.clone(),
            Arc::new(RouteEntry { id: r.id, public_model: r.public_model.clone(), targets }),
        );
    }
    Snapshot { providers, routes_by_model, fingerprint: raw.fingerprint() }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    current: ArcSwap<Snapshot>,
    store: Arc<dyn RegistryStore>,
    secrets: Arc<SecretStore>,
    reload_lock: tokio::sync::Mutex<()>,
}

impl Registry {
    /// 首次全量加载；失败时由调用方以非零码退出（需求 14 第 2 条）。
    pub async fn load_initial(
        store: Arc<dyn RegistryStore>,
        secrets: Arc<SecretStore>,
    ) -> Result<Registry, RegistryError> {
        let raw = store.load_all().await?;
        let snapshot = build_snapshot(&raw, &secrets);
        Ok(Registry {
            current: ArcSwap::from_pointee(snapshot),
            store,
            secrets,
            reload_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    /// 全量加载并替换快照；失败时保留旧快照。
    pub async fn reload(&self) -> Result<(), RegistryError> {
        let _guard = self.reload_lock.lock().await;
        self.reload_locked().await
    }

    async fn reload_locked(&self) -> Result<(), RegistryError> {
        let raw = self.store.load_all().await?;
        self.current.store(Arc::new(build_snapshot(&raw, &self.secrets)));
        Ok(())
    }

    /// 指纹与当前快照不同则全量重载，返回是否重载。
    pub async fn poll_once(&self) -> Result<bool, RegistryError> {
        let _guard = self.reload_lock.lock().await;
        let fp = self.store.fingerprint().await?;
        if fp == self.current.load().fingerprint {
            return Ok(false);
        }
        self.reload_locked().await?;
        Ok(true)
    }

    /// 每隔 `every` 轮询一次，直到 `shutdown` 被取消。失败只记 warn 日志并保留旧快照。
    pub fn spawn_poller(self: Arc<Self>, every: Duration, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        match self.poll_once().await {
                            Ok(true) => tracing::info!("registry reloaded after external change"),
                            Ok(false) => {}
                            Err(e) => tracing::warn!(error = %e, "registry poll failed; keeping previous snapshot"),
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::OsRandom;
    use crate::secret::SecretStore;
    use crate::testkit::InMemoryRegistryStore;

    fn secret_store() -> SecretStore {
        SecretStore::new(&[9; 32], Arc::new(OsRandom))
    }

    fn provider(secrets: &SecretStore, id: Uuid, name: &str, key: &str, updated_at: DateTime<Utc>) -> RawProvider {
        let sealed = secrets.seal(key.as_bytes());
        RawProvider {
            id,
            name: name.to_owned(),
            protocol: "openai".to_owned(),
            base_url: "https://api.example.com/v1".to_owned(),
            api_key_nonce: sealed.nonce.to_vec(),
            api_key_ciphertext: sealed.ciphertext,
            connect_timeout_s: 5,
            ttfb_timeout_s: 60,
            total_timeout_s: 600,
            enabled: true,
            updated_at,
        }
    }

    /// 需求 14.5：轮询/刷新失败时保留现有快照，不做任何部分替换。
    #[tokio::test]
    async fn reload_failure_keeps_old_snapshot() {
        let secrets = Arc::new(secret_store());
        let now = Utc::now();
        let p = provider(&secrets, Uuid::new_v4(), "p1", "key-1", now);
        let raw = RawData { providers: vec![p], routes: vec![], targets: vec![] };
        let store = Arc::new(InMemoryRegistryStore::new(raw));
        let registry = Registry::load_initial(store.clone(), secrets.clone()).await.unwrap();

        let before = registry.snapshot();
        assert_eq!(before.providers.len(), 1);

        store.set_failing(true);
        assert!(registry.reload().await.is_err());

        let after = registry.snapshot();
        // 同一个 Arc 指针：没有发生任何替换（即便是部分替换）。
        assert!(Arc::ptr_eq(&before, &after));
        assert_eq!(after.providers.len(), 1);

        // 恢复后可以重新成功加载。
        store.set_failing(false);
        assert!(registry.reload().await.is_ok());
    }

    /// 需求 14.6：请求早先持有的 `Arc<Snapshot>` 在快照被替换之后保持不变；
    /// 之后获取的快照反映最新数据。
    #[tokio::test]
    async fn held_snapshot_unaffected_by_later_replacement() {
        let secrets = Arc::new(secret_store());
        let now = Utc::now();
        let p = provider(&secrets, Uuid::new_v4(), "p1", "key-1", now);
        let raw = RawData { providers: vec![p], routes: vec![], targets: vec![] };
        let store = Arc::new(InMemoryRegistryStore::new(raw));
        let registry = Registry::load_initial(store.clone(), secrets.clone()).await.unwrap();

        let held = registry.snapshot();
        assert_eq!(held.providers.len(), 1);

        let p2 = provider(&secrets, Uuid::new_v4(), "p2", "key-2", now + chrono::Duration::seconds(1));
        store.mutate(|d| d.providers.push(p2));
        registry.reload().await.unwrap();

        // 先前持有的快照内容没有变化。
        assert_eq!(held.providers.len(), 1);
        // 新获取的快照已反映变更。
        let latest = registry.snapshot();
        assert_eq!(latest.providers.len(), 2);
        assert!(!Arc::ptr_eq(&held, &latest));
    }

    /// 需求 13.5：单个 Provider 的 Upstream_Key 解密失败只影响该 Provider，
    /// 其余 Provider 仍然可用。
    #[tokio::test]
    async fn single_provider_decrypt_failure_only_affects_that_provider() {
        let secrets = secret_store();
        let now = Utc::now();
        let good = provider(&secrets, Uuid::new_v4(), "good", "key-good", now);
        let mut bad = provider(&secrets, Uuid::new_v4(), "bad", "key-bad", now);
        bad.api_key_ciphertext[0] ^= 0x80;

        let raw = RawData { providers: vec![good.clone(), bad.clone()], routes: vec![], targets: vec![] };
        let snapshot = build_snapshot(&raw, &secrets);

        assert_eq!(snapshot.providers.len(), 2);
        assert!(snapshot.providers[&good.id].api_key.is_some());
        assert!(snapshot.providers[&good.id].is_available());
        assert!(snapshot.providers[&bad.id].api_key.is_none());
        assert!(!snapshot.providers[&bad.id].is_available());
    }
}
