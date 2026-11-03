//! Property 10: Admin 令牌比较
//!
//! **Validates: Requirements 8.2, 8.3, 8.5**

use llm_gateway::http::admin::{AdminState, admin_router};
use proptest::prelude::*;
use secrecy::SecretString;

/// 返回 (状态码, 响应体, 响应头中是否含令牌)。
async fn call(token: &str, auth: Option<&str>) -> (u16, Vec<u8>, bool) {
    let app = admin_router(AdminState::new(&SecretString::from(token.to_string())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut req = reqwest::Client::new().get(format!("http://{addr}/admin/providers"));
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let leaked = resp
        .headers()
        .iter()
        .any(|(k, v)| contains(k.as_str().as_bytes(), token) || contains(v.as_bytes(), token));
    let body = resp.bytes().await.unwrap().to_vec();
    server.abort();
    (status, body, leaked)
}

fn contains(hay: &[u8], needle: &str) -> bool {
    let n = needle.as_bytes();
    !n.is_empty() && hay.windows(n.len()).any(|w| w == n)
}

fn token_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(0x21u8..=0x7e, 1..=256)
        .prop_map(|v| String::from_utf8(v).unwrap())
}

fn swap_case(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
        .collect()
}

/// 构造候选头值：既含应放行的，也含各类应拒绝的。
fn header_strategy(t: String) -> impl Strategy<Value = Option<String>> {
    let scheme = proptest::collection::vec(any::<bool>(), 6).prop_map(|flags| {
        "bearer"
            .chars()
            .zip(flags)
            .map(|(c, up)| if up { c.to_ascii_uppercase() } else { c })
            .collect::<String>()
    });
    let len = t.len();
    (scheme, 0usize..7, 0..=len, "[!-~]{1,8}", 2usize..5).prop_map(
        move |(sch, kind, cut, suffix, spaces)| match kind {
            0 => None,
            1 | 2 => Some(format!("{sch} {t}")),
            3 => Some(format!("Basic {t}")),
            4 => Some(format!("{sch} {}", &t[..cut.min(len - 1)])),
            5 => Some(format!("{sch} {}", swap_case(&t))),
            _ => {
                if cut % 2 == 0 {
                    Some(format!("{sch} {t}{suffix}"))
                } else {
                    Some(format!("{sch}{}{t}", " ".repeat(spaces)))
                }
            }
        },
    )
}

fn is_accepted(h: &Option<String>, t: &str) -> bool {
    match h {
        Some(h) => {
            h.len() == 7 + t.len() && h.as_bytes()[..6].eq_ignore_ascii_case(b"bearer") && h.as_bytes()[6] == b' ' && &h[7..] == t
        }
        None => false,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn p10_admin_token_compare(
        (t, h) in token_strategy().prop_flat_map(|t| {
            let hs = header_strategy(t.clone());
            (Just(t), hs)
        })
    ) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (status, body, leaked) = call(&t, h.as_deref()).await;
            if is_accepted(&h, &t) {
                // 放行：到达路由（未实现路由返回 404），而不是 401。
                prop_assert_ne!(status, 401);
            } else {
                prop_assert_eq!(status, 401, "h={:?}", h);
                prop_assert!(!leaked, "token leaked in headers");
                prop_assert!(!contains(&body, &t), "token leaked in body");
                // 与"缺失头"的基准拒绝响应字节相同。
                let (s0, b0, _) = call(&t, None).await;
                prop_assert_eq!(s0, 401);
                prop_assert_eq!(body, b0);
            }
            Ok(())
        })?;
    }
}
