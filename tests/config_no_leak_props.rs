//! Property 5: 配置错误不泄露机密。
//! **Validates: Requirements 1.7**

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use llm_gateway::config::Config;
use proptest::prelude::*;

fn marker() -> impl Strategy<Value = String> {
    "[A-Za-z0-9]{8,24}".prop_map(|s| format!("SECRET{s}"))
}

/// 导致错误的非机密变量（至少一个）。
fn bad_vars() -> impl Strategy<Value = Vec<(&'static str, String)>> {
    let bad = prop_oneof![
        Just(("GATEWAY_MAX_ATTEMPTS", "11".to_string())),
        Just(("GATEWAY_MAX_ATTEMPTS", "abc".to_string())),
        Just(("GATEWAY_SHUTDOWN_GRACE_SECS", "-1".to_string())),
        Just(("GATEWAY_BREAKER_THRESHOLD", "0".to_string())),
        Just(("GATEWAY_RATELIMIT_FAIL_OPEN", "yes".to_string())),
        Just(("GATEWAY_LISTEN_ADDR", "localhost".to_string())),
        Just(("GATEWAY_METRICS_ADDR", "nope".to_string())),
        Just(("GATEWAY_OUTPUT_RESERVE_CAP", "99999999999999999999999".to_string())),
    ];
    prop::collection::vec(bad, 1..5)
}

proptest! {
    // 主密钥非法（含非 Base64 字符）或长度错误，叠加其他错误
    #[test]
    fn p05_errors_redact_secrets(
        master in marker(),
        admin in marker(),
        db_pw in marker(),
        redis_pw in marker(),
        wrong_len in prop::collection::vec(any::<u8>(), 8..31),
        use_wrong_len in any::<bool>(),
        valid_key in any::<[u8; 32]>(),
        key_valid in any::<bool>(),
        bad in bad_vars(),
        bad_master_too in any::<bool>(),
    ) {
        let master_val = if key_valid && !bad_master_too {
            STANDARD.encode(valid_key)
        } else if use_wrong_len {
            STANDARD.encode(&wrong_len)
        } else {
            master.clone()
        };
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("DATABASE_URL".into(), format!("postgres://user:{db_pw}@db:5432/app"));
        vars.insert("REDIS_URL".into(), format!("redis://:{redis_pw}@redis:6379"));
        vars.insert("GATEWAY_MASTER_KEY".into(), master_val.clone());
        vars.insert("GATEWAY_ADMIN_TOKEN".into(), admin.clone());
        for (k, v) in bad {
            vars.insert(k.to_string(), v);
        }

        let errs = Config::from_vars(&vars).expect_err("configuration must be invalid");
        let text: String = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
        let debug = format!("{errs:?}");

        for secret in [&master, &admin, &db_pw, &redis_pw, &master_val] {
            prop_assert!(!text.contains(secret.as_str()), "Display leaked {secret}: {text}");
            prop_assert!(!debug.contains(secret.as_str()), "Debug leaked {secret}: {debug}");
        }
    }

    // 缺失变量的情形：已设置的机密同样不出现在错误里
    #[test]
    fn p05_missing_errors_redact_secrets(
        admin in marker(),
        db_pw in marker(),
        valid_key in any::<[u8; 32]>(),
    ) {
        let key = STANDARD.encode(valid_key);
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("DATABASE_URL".into(), format!("postgres://user:{db_pw}@db/app"));
        vars.insert("GATEWAY_MASTER_KEY".into(), key.clone());
        vars.insert("GATEWAY_ADMIN_TOKEN".into(), admin.clone());
        // REDIS_URL 缺失
        let errs = Config::from_vars(&vars).expect_err("missing REDIS_URL");
        let text: String = errs.iter().map(|e| e.to_string()).collect();
        for secret in [&admin, &db_pw, &key] {
            prop_assert!(!text.contains(secret.as_str()));
        }
    }
}
