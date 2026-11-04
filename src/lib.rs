//! LLM Gateway 库入口。模块划分见 design.md“模块划分”。

pub mod auth;
pub mod breaker;
pub mod clock;
pub mod config;
pub mod db;
pub mod http;
pub mod ir;
pub mod logging;
pub mod metrics;
pub mod protocol;
pub mod proxy;
pub mod ratelimit;
pub mod registry;
pub mod secret;
pub mod select;
pub mod sse;
pub mod tokens;
pub mod usage;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

/// 组装并启动网关（具体启动流程在后续任务中实现）。
pub async fn run() {
    let config = match config::Config::from_env() {
        Ok(c) => c,
        Err(errors) => {
            for e in &errors {
                eprintln!("{e}");
            }
            std::process::exit(1);
        }
    };
    logging::init();

    use secrecy::ExposeSecret;
    let pg = match db::connect_and_migrate(config.database_url.expose_secret()).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "DATABASE_URL: failed to connect or migrate ({}): {e}",
                config::redact_url(config.database_url.expose_secret())
            );
            std::process::exit(1);
        }
    };

    let secrets = std::sync::Arc::new(secret::SecretStore::new(
        config.master_key.expose_secret(),
        std::sync::Arc::new(clock::OsRandom),
    ));
    let registry = match registry::Registry::load_initial(
        std::sync::Arc::new(registry::PgRegistryStore::new(pg.clone())),
        secrets.clone(),
    )
    .await
    {
        Ok(r) => std::sync::Arc::new(r),
        Err(e) => {
            eprintln!("registry: initial load failed: {e}");
            std::process::exit(1);
        }
    };

    let listener = match tokio::net::TcpListener::bind(config.listen_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("GATEWAY_LISTEN_ADDR: failed to bind: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(addr = %config.listen_addr, "listening");
    // 关闭信号在优雅关闭任务中接入；此前令牌不会被取消。
    let shutdown = tokio_util::sync::CancellationToken::new();
    let _registry_poller = registry.clone().spawn_poller(config.registry_poll, shutdown.clone());
    let admin = http::admin::AdminState::new(&config.admin_token).with_db(pg.clone(), secrets).with_registry(registry.clone());
    let probes = vec![http::health::Probe::postgres(pg)];
    let app = http::build_router_full(Some(admin), Some(probes), Some(registry.clone()));
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "server error");
        std::process::exit(1);
    }
}
