//! Config_Loader 单元测试（需求 1.3、1.4）。

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use llm_gateway::config::{Config, ConfigError};
use secrecy::ExposeSecret;

const KEY_B64: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc="; // 32 x 0x07

fn base() -> HashMap<String, String> {
    [
        ("DATABASE_URL", "postgres://u:dbpass@h/db"),
        ("REDIS_URL", "redis://:rpass@h:6379"),
        ("GATEWAY_MASTER_KEY", KEY_B64),
        ("GATEWAY_ADMIN_TOKEN", "tok"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn with(name: &str, value: &str) -> Config {
    let mut v = base();
    v.insert(name.to_string(), value.to_string());
    Config::from_vars(&v).unwrap()
}

#[test]
fn required_vars_valid() {
    let c = Config::from_vars(&base()).unwrap();
    assert_eq!(c.database_url.expose_secret(), "postgres://u:dbpass@h/db");
    assert_eq!(c.redis_url.expose_secret(), "redis://:rpass@h:6379");
    assert_eq!(c.admin_token.expose_secret(), "tok");
    assert_eq!(c.master_key.expose_secret(), &[7u8; 32]);
}

#[test]
fn listen_addr_valid() {
    assert_eq!(with("GATEWAY_LISTEN_ADDR", "127.0.0.1:9000").listen_addr, "127.0.0.1:9000".parse().unwrap());
}

#[test]
fn metrics_addr_valid() {
    assert_eq!(
        with("GATEWAY_METRICS_ADDR", "127.0.0.1:9100").metrics_addr,
        Some("127.0.0.1:9100".parse().unwrap())
    );
}

#[test]
fn registry_poll_valid() {
    assert_eq!(with("GATEWAY_REGISTRY_POLL_SECS", "60").registry_poll, Duration::from_secs(60));
}

#[test]
fn key_cache_ttl_valid() {
    assert_eq!(with("GATEWAY_KEY_CACHE_TTL_SECS", "120").key_cache_ttl, Duration::from_secs(120));
}

#[test]
fn output_reserve_cap_valid() {
    assert_eq!(with("GATEWAY_OUTPUT_RESERVE_CAP", "2048").output_reserve_cap, 2048);
}

#[test]
fn ratelimit_fail_open_valid() {
    assert!(!with("GATEWAY_RATELIMIT_FAIL_OPEN", "false").ratelimit_fail_open);
    assert!(with("GATEWAY_RATELIMIT_FAIL_OPEN", "true").ratelimit_fail_open);
}

#[test]
fn max_attempts_valid() {
    assert_eq!(with("GATEWAY_MAX_ATTEMPTS", "7").max_attempts, 7);
}

#[test]
fn breaker_threshold_valid() {
    assert_eq!(with("GATEWAY_BREAKER_THRESHOLD", "50").breaker_threshold, 50);
}

#[test]
fn breaker_cooldown_valid() {
    assert_eq!(with("GATEWAY_BREAKER_COOLDOWN_SECS", "90").breaker_cooldown, Duration::from_secs(90));
}

#[test]
fn shutdown_grace_valid() {
    assert_eq!(with("GATEWAY_SHUTDOWN_GRACE_SECS", "5").shutdown_grace, Duration::from_secs(5));
}

#[test]
fn missing_required_reported_together() {
    let errs = Config::from_vars(&HashMap::new()).unwrap_err();
    assert_eq!(
        errs,
        vec![ConfigError::Missing(vec![
            "DATABASE_URL",
            "REDIS_URL",
            "GATEWAY_MASTER_KEY",
            "GATEWAY_ADMIN_TOKEN"
        ])]
    );
}

#[test]
fn startup_failure_prints_to_stderr_and_exits_nonzero() {
    let out = Command::new(env!("CARGO_BIN_EXE_llm-gateway"))
        .env_clear()
        .env("GATEWAY_MAX_ATTEMPTS", "99")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing required environment variables: DATABASE_URL, REDIS_URL, GATEWAY_MASTER_KEY, GATEWAY_ADMIN_TOKEN"),
        "stderr: {stderr}"
    );
}

#[test]
fn startup_failure_range_error_message_on_stderr() {
    let out = Command::new(env!("CARGO_BIN_EXE_llm-gateway"))
        .env_clear()
        .env("DATABASE_URL", "postgres://u:dbpass@h/db")
        .env("REDIS_URL", "redis://:rpass@h:6379")
        .env("GATEWAY_MASTER_KEY", KEY_B64)
        .env("GATEWAY_ADMIN_TOKEN", "tok")
        .env("GATEWAY_MAX_ATTEMPTS", "99")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("GATEWAY_MAX_ATTEMPTS: value out of range, must be between 1 and 10 (inclusive)"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("dbpass") && !stderr.contains("rpass"));
}
