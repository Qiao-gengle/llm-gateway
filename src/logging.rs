//! tracing 初始化：单行 JSON 输出，RFC 3339 UTC 时间戳（需求 34.1）。

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

/// RFC 3339 UTC 时间戳（毫秒精度，`Z` 后缀）。
#[derive(Clone, Copy, Debug, Default)]
pub struct Rfc3339Utc;

impl FormatTime for Rfc3339Utc {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        )
    }
}

/// 初始化全局 JSON 日志；重复调用时静默忽略。
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_timer(Rfc3339Utc)
        .with_current_span(true)
        .flatten_event(true)
        .with_ansi(false)
        .try_init();
}
