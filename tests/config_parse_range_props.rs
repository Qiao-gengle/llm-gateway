//! Property 4: 配置解析与范围校验
//!
//! **Validates: Requirements 1.4, 1.5, 1.6**

use std::collections::HashMap;
use std::net::SocketAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use llm_gateway::config::{Config, ConfigError};
use proptest::prelude::*;

/// (变量名, min, max)
const INT_VARS: [(&str, u64, u64); 7] = [
    ("GATEWAY_REGISTRY_POLL_SECS", 1, 3600),
    ("GATEWAY_KEY_CACHE_TTL_SECS", 1, 3600),
    ("GATEWAY_OUTPUT_RESERVE_CAP", 1, 1_000_000),
    ("GATEWAY_MAX_ATTEMPTS", 1, 10),
    ("GATEWAY_BREAKER_THRESHOLD", 1, 1000),
    ("GATEWAY_BREAKER_COOLDOWN_SECS", 1, 3600),
    ("GATEWAY_SHUTDOWN_GRACE_SECS", 0, 3600),
];

fn base() -> HashMap<String, String> {
    [
        ("DATABASE_URL", "postgres://u:p@h/db".to_string()),
        ("REDIS_URL", "redis://h:6379".to_string()),
        ("GATEWAY_MASTER_KEY", STANDARD.encode([7u8; 32])),
        ("GATEWAY_ADMIN_TOKEN", "tok".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn int_value() -> impl Strategy<Value = String> {
    prop_oneof![
        // 数字串：覆盖范围内、边界附近、越界与超出 u64
        (0u64..=1_100_000).prop_map(|n| n.to_string()),
        "[0-9]{1,25}",
        // 任意字符串（含非数字、负号、空白、Unicode）
        any::<String>(),
        "[-+ 0-9a-z.]{0,8}",
    ]
}

proptest! {
    #[test]
    fn p04_parse_and_range(idx in 0usize..INT_VARS.len(), s in int_value()) {
        let (var, lo, hi) = INT_VARS[idx];
        let mut vars = base();
        vars.insert(var.to_string(), s.clone());
        let res = Config::from_vars(&vars);

        if s.is_empty() {
            // 空串视为未设置，使用默认值
            prop_assert!(res.is_ok());
        } else if s.bytes().all(|b| b.is_ascii_digit()) {
            match s.parse::<u64>() {
                Ok(n) if (lo..=hi).contains(&n) => prop_assert!(res.is_ok(), "{s} should be accepted"),
                _ => prop_assert_eq!(
                    res.unwrap_err(),
                    vec![ConfigError::OutOfRange { var: var_static(idx), min: lo, max: hi }]
                ),
            }
        } else {
            let errs = res.unwrap_err();
            prop_assert_eq!(errs.len(), 1);
            match &errs[0] {
                ConfigError::Parse { var: v, .. } => prop_assert_eq!(*v, var),
                other => prop_assert!(false, "expected Parse, got {:?}", other),
            }
        }
    }

    #[test]
    fn p04_master_key_length(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let mut vars = base();
        vars.insert("GATEWAY_MASTER_KEY".into(), STANDARD.encode(&bytes));
        let res = Config::from_vars(&vars);
        if bytes.len() == 32 {
            prop_assert!(res.is_ok());
        } else if bytes.is_empty() {
            // 空串等同未设置
            prop_assert_eq!(res.unwrap_err(), vec![ConfigError::Missing(vec!["GATEWAY_MASTER_KEY"])]);
        } else {
            prop_assert_eq!(res.unwrap_err(), vec![ConfigError::MasterKey]);
        }
    }

    #[test]
    fn p04_master_key_not_base64(s in any::<String>()) {
        prop_assume!(!s.is_empty() && STANDARD.decode(&s).is_err());
        let mut vars = base();
        vars.insert("GATEWAY_MASTER_KEY".into(), s);
        prop_assert_eq!(Config::from_vars(&vars).unwrap_err(), vec![ConfigError::MasterKey]);
    }

    #[test]
    fn p04_bad_socket_addr(
        s in prop_oneof![any::<String>(), "[a-z0-9.:]{1,20}"],
        listen in any::<bool>(),
    ) {
        prop_assume!(!s.is_empty() && s.parse::<SocketAddr>().is_err());
        let var = if listen { "GATEWAY_LISTEN_ADDR" } else { "GATEWAY_METRICS_ADDR" };
        let mut vars = base();
        vars.insert(var.into(), s);
        let errs = Config::from_vars(&vars).unwrap_err();
        prop_assert_eq!(errs.len(), 1);
        match &errs[0] {
            ConfigError::Parse { var: v, .. } => prop_assert_eq!(*v, var),
            other => prop_assert!(false, "expected Parse, got {:?}", other),
        }
    }

    #[test]
    fn p04_valid_socket_addr_accepted(
        ip in any::<[u8; 4]>(),
        port in any::<u16>(),
    ) {
        let addr = SocketAddr::from((ip, port));
        let mut vars = base();
        vars.insert("GATEWAY_LISTEN_ADDR".into(), addr.to_string());
        vars.insert("GATEWAY_METRICS_ADDR".into(), addr.to_string());
        let c = Config::from_vars(&vars).unwrap();
        prop_assert_eq!(c.listen_addr, addr);
        prop_assert_eq!(c.metrics_addr, Some(addr));
    }
}

fn var_static(idx: usize) -> &'static str {
    INT_VARS[idx].0
}
