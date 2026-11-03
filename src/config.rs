//! Config_Loader：启动时从环境变量读取并校验配置（需求 1）。

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use secrecy::{SecretBox, SecretString};

const LISTEN_ADDR: &str = "GATEWAY_LISTEN_ADDR";
const METRICS_ADDR: &str = "GATEWAY_METRICS_ADDR";
const DATABASE_URL: &str = "DATABASE_URL";
const REDIS_URL: &str = "REDIS_URL";
const MASTER_KEY: &str = "GATEWAY_MASTER_KEY";
const ADMIN_TOKEN: &str = "GATEWAY_ADMIN_TOKEN";
const REGISTRY_POLL_SECS: &str = "GATEWAY_REGISTRY_POLL_SECS";
const KEY_CACHE_TTL_SECS: &str = "GATEWAY_KEY_CACHE_TTL_SECS";
const OUTPUT_RESERVE_CAP: &str = "GATEWAY_OUTPUT_RESERVE_CAP";
const RATELIMIT_FAIL_OPEN: &str = "GATEWAY_RATELIMIT_FAIL_OPEN";
const MAX_ATTEMPTS: &str = "GATEWAY_MAX_ATTEMPTS";
const BREAKER_THRESHOLD: &str = "GATEWAY_BREAKER_THRESHOLD";
const BREAKER_COOLDOWN_SECS: &str = "GATEWAY_BREAKER_COOLDOWN_SECS";
const SHUTDOWN_GRACE_SECS: &str = "GATEWAY_SHUTDOWN_GRACE_SECS";

/// 全部受管环境变量名（用于 `from_env` 构造快照）。
const ALL_VARS: [&str; 14] = [
    LISTEN_ADDR,
    METRICS_ADDR,
    DATABASE_URL,
    REDIS_URL,
    MASTER_KEY,
    ADMIN_TOKEN,
    REGISTRY_POLL_SECS,
    KEY_CACHE_TTL_SECS,
    OUTPUT_RESERVE_CAP,
    RATELIMIT_FAIL_OPEN,
    MAX_ATTEMPTS,
    BREAKER_THRESHOLD,
    BREAKER_COOLDOWN_SECS,
    SHUTDOWN_GRACE_SECS,
];

#[derive(Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub metrics_addr: Option<SocketAddr>,
    pub database_url: SecretString,
    pub redis_url: SecretString,
    pub master_key: SecretBox<[u8; 32]>,
    pub admin_token: SecretString,
    pub registry_poll: Duration,
    pub key_cache_ttl: Duration,
    pub output_reserve_cap: u32,
    pub ratelimit_fail_open: bool,
    pub max_attempts: u32,
    pub breaker_threshold: u32,
    pub breaker_cooldown: Duration,
    pub shutdown_grace: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// 一次列出全部缺失（未设置或为空串）的必填变量。
    Missing(Vec<&'static str>),
    Parse { var: &'static str, reason: String },
    OutOfRange { var: &'static str, min: u64, max: u64 },
    MasterKey,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(vars) => {
                write!(f, "missing required environment variables: {}", vars.join(", "))
            }
            ConfigError::Parse { var, reason } => write!(f, "{var}: {reason}"),
            ConfigError::OutOfRange { var, min, max } => {
                write!(f, "{var}: value out of range, must be between {min} and {max} (inclusive)")
            }
            ConfigError::MasterKey => {
                write!(f, "{MASTER_KEY}: must be base64 of 32 bytes")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// 去除 URL 中 userinfo 的密码部分（`scheme://user:pass@host` → `scheme://user@host`）。
/// 不含密码的输入原样返回。
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let auth_start = scheme_end + 3;
    let rest = &url[auth_start..];
    let auth_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_len];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };
    format!(
        "{}{}{}",
        &url[..auth_start],
        &userinfo[..colon],
        &url[auth_start + at..]
    )
}

fn get<'a>(vars: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    vars.get(name).map(String::as_str).filter(|v| !v.is_empty())
}

fn parse_addr(var: &'static str, raw: &str) -> Result<SocketAddr, ConfigError> {
    raw.parse::<SocketAddr>().map_err(|_| ConfigError::Parse {
        var,
        reason: "must be a socket address in IP:port form".to_string(),
    })
}

fn parse_int(
    var: &'static str,
    raw: Option<&str>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ConfigError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ConfigError::Parse {
            var,
            reason: "must be a non-negative decimal integer".to_string(),
        });
    }
    // 全为数字但超出 u64：必然越界。
    let n = raw.parse::<u64>().map_err(|_| ConfigError::OutOfRange { var, min, max })?;
    if n < min || n > max {
        return Err(ConfigError::OutOfRange { var, min, max });
    }
    Ok(n)
}

impl Config {
    /// 纯函数：输入为环境变量快照。
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Config, Vec<ConfigError>> {
        let missing: Vec<&'static str> = [DATABASE_URL, REDIS_URL, MASTER_KEY, ADMIN_TOKEN]
            .into_iter()
            .filter(|v| get(vars, v).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(vec![ConfigError::Missing(missing)]);
        }

        let mut errors = Vec::new();
        let mut collect = |r: Result<u64, ConfigError>| match r {
            Ok(v) => v,
            Err(e) => {
                errors.push(e);
                0
            }
        };

        let registry_poll = collect(parse_int(REGISTRY_POLL_SECS, get(vars, REGISTRY_POLL_SECS), 10, 1, 3600));
        let key_cache_ttl = collect(parse_int(KEY_CACHE_TTL_SECS, get(vars, KEY_CACHE_TTL_SECS), 30, 1, 3600));
        let output_reserve_cap =
            collect(parse_int(OUTPUT_RESERVE_CAP, get(vars, OUTPUT_RESERVE_CAP), 1024, 1, 1_000_000));
        let max_attempts = collect(parse_int(MAX_ATTEMPTS, get(vars, MAX_ATTEMPTS), 3, 1, 10));
        let breaker_threshold =
            collect(parse_int(BREAKER_THRESHOLD, get(vars, BREAKER_THRESHOLD), 5, 1, 1000));
        let breaker_cooldown =
            collect(parse_int(BREAKER_COOLDOWN_SECS, get(vars, BREAKER_COOLDOWN_SECS), 30, 1, 3600));
        let shutdown_grace =
            collect(parse_int(SHUTDOWN_GRACE_SECS, get(vars, SHUTDOWN_GRACE_SECS), 30, 0, 3600));

        let listen_addr = match get(vars, LISTEN_ADDR) {
            None => SocketAddr::from(([0, 0, 0, 0], 8080)),
            Some(raw) => parse_addr(LISTEN_ADDR, raw).unwrap_or_else(|e| {
                errors.push(e);
                SocketAddr::from(([0, 0, 0, 0], 0))
            }),
        };
        let metrics_addr = match get(vars, METRICS_ADDR) {
            None => None,
            Some(raw) => match parse_addr(METRICS_ADDR, raw) {
                Ok(a) => Some(a),
                Err(e) => {
                    errors.push(e);
                    None
                }
            },
        };

        let ratelimit_fail_open = match get(vars, RATELIMIT_FAIL_OPEN) {
            None => true,
            Some("true") => true,
            Some("false") => false,
            Some(_) => {
                errors.push(ConfigError::Parse {
                    var: RATELIMIT_FAIL_OPEN,
                    reason: "must be `true` or `false`".to_string(),
                });
                true
            }
        };

        let mut master = [0u8; 32];
        let master_ok = match STANDARD.decode(get(vars, MASTER_KEY).unwrap_or_default()) {
            Ok(bytes) if bytes.len() == 32 => {
                master.copy_from_slice(&bytes);
                true
            }
            _ => false,
        };
        if !master_ok {
            errors.push(ConfigError::MasterKey);
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let secret = |name: &str| SecretString::from(get(vars, name).unwrap_or_default().to_string());
        Ok(Config {
            listen_addr,
            metrics_addr,
            database_url: secret(DATABASE_URL),
            redis_url: secret(REDIS_URL),
            master_key: SecretBox::new(Box::new(master)),
            admin_token: secret(ADMIN_TOKEN),
            registry_poll: Duration::from_secs(registry_poll),
            key_cache_ttl: Duration::from_secs(key_cache_ttl),
            output_reserve_cap: output_reserve_cap as u32,
            ratelimit_fail_open,
            max_attempts: max_attempts as u32,
            breaker_threshold: breaker_threshold as u32,
            breaker_cooldown: Duration::from_secs(breaker_cooldown),
            shutdown_grace: Duration::from_secs(shutdown_grace),
        })
    }

    pub fn from_env() -> Result<Config, Vec<ConfigError>> {
        let vars: HashMap<String, String> = ALL_VARS
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|v| (name.to_string(), v)))
            .collect();
        Config::from_vars(&vars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn base() -> HashMap<String, String> {
        let key = STANDARD.encode([7u8; 32]);
        [
            (DATABASE_URL, "postgres://u:dbpass@h/db"),
            (REDIS_URL, "redis://:rpass@h:6379"),
            (MASTER_KEY, key.as_str()),
            (ADMIN_TOKEN, "tok"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn defaults_apply_and_empty_is_unset() {
        let mut v = base();
        v.insert(LISTEN_ADDR.into(), String::new());
        v.insert(MAX_ATTEMPTS.into(), String::new());
        let c = Config::from_vars(&v).unwrap();
        assert_eq!(c.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.metrics_addr, None);
        assert_eq!(c.registry_poll, Duration::from_secs(10));
        assert_eq!(c.key_cache_ttl, Duration::from_secs(30));
        assert_eq!(c.output_reserve_cap, 1024);
        assert!(c.ratelimit_fail_open);
        assert_eq!(c.max_attempts, 3);
        assert_eq!(c.breaker_threshold, 5);
        assert_eq!(c.breaker_cooldown, Duration::from_secs(30));
        assert_eq!(c.shutdown_grace, Duration::from_secs(30));
        assert_eq!(c.admin_token.expose_secret(), "tok");
    }

    #[test]
    fn all_missing_listed_at_once() {
        let mut v = base();
        v.remove(DATABASE_URL);
        v.insert(ADMIN_TOKEN.into(), String::new());
        let errs = Config::from_vars(&v).unwrap_err();
        assert_eq!(errs, vec![ConfigError::Missing(vec![DATABASE_URL, ADMIN_TOKEN])]);
    }

    #[test]
    fn parse_and_range_errors() {
        let mut v = base();
        v.insert(MAX_ATTEMPTS.into(), "11".into());
        v.insert(SHUTDOWN_GRACE_SECS.into(), "-1".into());
        v.insert(RATELIMIT_FAIL_OPEN.into(), "yes".into());
        v.insert(METRICS_ADDR.into(), "localhost".into());
        v.insert(MASTER_KEY.into(), "c2hvcnQ=".into());
        let errs = Config::from_vars(&v).unwrap_err();
        assert_eq!(errs.len(), 5);
        assert!(errs.contains(&ConfigError::OutOfRange { var: MAX_ATTEMPTS, min: 1, max: 10 }));
        assert!(errs.contains(&ConfigError::MasterKey));
        let text: String = errs.iter().map(|e| e.to_string()).collect();
        assert!(!text.contains("c2hvcnQ="));
    }

    #[test]
    fn boundaries_accepted() {
        let mut v = base();
        v.insert(SHUTDOWN_GRACE_SECS.into(), "0".into());
        v.insert(OUTPUT_RESERVE_CAP.into(), "1000000".into());
        let c = Config::from_vars(&v).unwrap();
        assert_eq!(c.shutdown_grace, Duration::ZERO);
        assert_eq!(c.output_reserve_cap, 1_000_000);
    }

    #[test]
    fn redact_url_removes_password() {
        assert_eq!(redact_url("postgres://u:dbpass@h:5432/db"), "postgres://u@h:5432/db");
        assert_eq!(redact_url("redis://:rpass@h:6379"), "redis://@h:6379");
        assert_eq!(redact_url("redis://h:6379"), "redis://h:6379");
        assert_eq!(redact_url("postgres://u@h/db"), "postgres://u@h/db");
    }
}
