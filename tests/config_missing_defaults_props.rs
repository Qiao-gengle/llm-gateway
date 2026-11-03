//! Feature: llm-gateway, Property 3: 配置缺失项与默认值
//!
//! **Validates: Requirements 1.2, 1.3**

use std::collections::HashMap;
use std::time::Duration;

use llm_gateway::config::{Config, ConfigError};
use proptest::prelude::*;

const REQUIRED: [&str; 4] = [
    "DATABASE_URL",
    "REDIS_URL",
    "GATEWAY_MASTER_KEY",
    "GATEWAY_ADMIN_TOKEN",
];
const OPTIONAL: [&str; 10] = [
    "GATEWAY_LISTEN_ADDR",
    "GATEWAY_METRICS_ADDR",
    "GATEWAY_REGISTRY_POLL_SECS",
    "GATEWAY_KEY_CACHE_TTL_SECS",
    "GATEWAY_OUTPUT_RESERVE_CAP",
    "GATEWAY_RATELIMIT_FAIL_OPEN",
    "GATEWAY_MAX_ATTEMPTS",
    "GATEWAY_BREAKER_THRESHOLD",
    "GATEWAY_BREAKER_COOLDOWN_SECS",
    "GATEWAY_SHUTDOWN_GRACE_SECS",
];

/// base64 of 32 zero bytes.
const VALID_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

fn valid_value(name: &str) -> String {
    match name {
        "DATABASE_URL" => "postgres://u:p@h/db".to_string(),
        "REDIS_URL" => "redis://h:6379".to_string(),
        "GATEWAY_MASTER_KEY" => VALID_KEY.to_string(),
        _ => "tok".to_string(),
    }
}

/// 0 = valid, 1 = unset, 2 = empty
fn required_state() -> impl Strategy<Value = u8> {
    0u8..3
}

/// 0 = unset, 1 = empty
fn optional_state() -> impl Strategy<Value = u8> {
    0u8..2
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn missing_required_and_defaults(
        req in proptest::collection::vec(required_state(), 4),
        opt in proptest::collection::vec(optional_state(), 10),
    ) {
        let mut vars: HashMap<String, String> = HashMap::new();
        let mut expected_missing: Vec<&str> = Vec::new();
        for (name, st) in REQUIRED.iter().zip(&req) {
            match st {
                0 => { vars.insert(name.to_string(), valid_value(name)); }
                1 => expected_missing.push(name),
                _ => { vars.insert(name.to_string(), String::new()); expected_missing.push(name); }
            }
        }
        for (name, st) in OPTIONAL.iter().zip(&opt) {
            if *st == 1 {
                vars.insert(name.to_string(), String::new());
            }
        }

        let result = Config::from_vars(&vars);
        if !expected_missing.is_empty() {
            let errs = result.expect_err("missing required must fail");
            let missing: Vec<&Vec<&'static str>> = errs
                .iter()
                .filter_map(|e| match e { ConfigError::Missing(v) => Some(v), _ => None })
                .collect();
            prop_assert_eq!(missing.len(), 1);
            let mut got: Vec<&str> = missing[0].clone();
            got.sort();
            let mut want = expected_missing.clone();
            want.sort();
            prop_assert_eq!(got, want);
        } else {
            let c = result.expect("all required present must succeed");
            prop_assert_eq!(c.listen_addr, "0.0.0.0:8080".parse().unwrap());
            prop_assert_eq!(c.metrics_addr, None);
            prop_assert_eq!(c.registry_poll, Duration::from_secs(10));
            prop_assert_eq!(c.key_cache_ttl, Duration::from_secs(30));
            prop_assert_eq!(c.output_reserve_cap, 1024);
            prop_assert!(c.ratelimit_fail_open);
            prop_assert_eq!(c.max_attempts, 3);
            prop_assert_eq!(c.breaker_threshold, 5);
            prop_assert_eq!(c.breaker_cooldown, Duration::from_secs(30));
            prop_assert_eq!(c.shutdown_grace, Duration::from_secs(30));
        }
    }
}
