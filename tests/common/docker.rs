//! 依赖 Docker 的测试基础设施：启动 Postgres 容器；Docker 不可用时跳过测试。

use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

/// 运行中的 Postgres 容器；`Drop` 时容器被移除。
pub struct PgContainer {
    _container: ContainerAsync<Postgres>,
    pub url: String,
}

/// 启动 `postgres:16`（与 docker-compose.yml 一致）。任何启动错误都以 `Err` 返回。
pub async fn start_postgres() -> Result<PgContainer, String> {
    let container = Postgres::default()
        .with_tag("16")
        .start()
        .await
        .map_err(|e| e.to_string())?;
    let host = container.get_host().await.map_err(|e| e.to_string())?;
    let port = container.get_host_port_ipv4(5432).await.map_err(|e| e.to_string())?;
    Ok(PgContainer {
        url: format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        _container: container,
    })
}

/// 启动容器并返回 `PgContainer`；失败时向 stderr 输出 `skipped: docker unavailable` 并从当前测试函数 `return`。
#[macro_export]
macro_rules! docker_or_skip {
    () => {
        match $crate::common::docker::start_postgres().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipped: docker unavailable");
                eprintln!("  cause: {e}");
                return;
            }
        }
    };
}
