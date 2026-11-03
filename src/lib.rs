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
pub async fn run() {}
