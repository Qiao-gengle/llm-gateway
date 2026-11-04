//! 测试接缝：内存实现（见 design.md“测试策略”）。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::db::{DbError, DbErrorKind};
use crate::registry::{Fingerprint, RawData, RegistryError, RegistryStore};

/// 内存 `RegistryStore`。指纹由当前数据计算（行数与最大 `updated_at`），
/// 因此修改数据时需要同时更新 `updated_at`，与触发器的行为一致。
#[derive(Default)]
pub struct InMemoryRegistryStore {
    data: Mutex<RawData>,
    failing: AtomicBool,
    load_count: AtomicUsize,
}

impl InMemoryRegistryStore {
    pub fn new(data: RawData) -> Self {
        Self { data: Mutex::new(data), ..Default::default() }
    }

    /// 修改存储的数据。
    pub fn mutate(&self, f: impl FnOnce(&mut RawData)) {
        f(&mut self.data.lock().unwrap());
    }

    /// 为 `true` 时 `load_all` 与 `fingerprint` 都返回连接错误。
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// `load_all` 被调用的次数（含失败的调用）。
    pub fn load_count(&self) -> usize {
        self.load_count.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), RegistryError> {
        if self.failing.load(Ordering::SeqCst) {
            Err(RegistryError::Store(DbError::new(DbErrorKind::Connection)))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl RegistryStore for InMemoryRegistryStore {
    async fn load_all(&self) -> Result<RawData, RegistryError> {
        self.load_count.fetch_add(1, Ordering::SeqCst);
        self.check()?;
        Ok(self.data.lock().unwrap().clone())
    }

    async fn fingerprint(&self) -> Result<Fingerprint, RegistryError> {
        self.check()?;
        Ok(self.data.lock().unwrap().fingerprint())
    }
}
