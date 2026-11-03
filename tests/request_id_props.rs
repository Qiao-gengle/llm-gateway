//! Property 6: Request_ID 解析
//! **Validates: Requirements 35.1, 35.2, 35.4, 35.5**

use axum::http::{HeaderMap, HeaderValue};
use llm_gateway::http::request_id::{REQUEST_ID_HEADER, RequestIdGenerator, resolve_request_id};
use proptest::prelude::*;

fn valid_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0x21u8..=0x7E, 1..=128)
}

/// 混合合法与各类非法（空、超长、含空格/制表/高位字节）的头值。
fn any_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => valid_bytes(),
        1 => Just(Vec::new()),
        1 => prop::collection::vec(0x21u8..=0x7E, 129..=200),
        2 => prop::collection::vec(prop_oneof![0x21u8..=0x7E, Just(b' '), Just(b'\t'), 0x80u8..=0xFF], 1..=40),
    ]
}

fn build(vals: &[Vec<u8>]) -> Option<HeaderMap> {
    let mut h = HeaderMap::new();
    for v in vals {
        h.append(REQUEST_ID_HEADER, HeaderValue::from_bytes(v).ok()?);
    }
    Some(h)
}

fn is_valid(v: &[u8]) -> bool {
    (1..=128).contains(&v.len()) && v.iter().all(|b| (0x21..=0x7E).contains(b))
}

proptest! {
    #[test]
    fn p06_request_id_resolution(vals in prop::collection::vec(any_bytes(), 0..=3)) {
        let Some(h) = build(&vals) else { return Ok(()); };
        let g = RequestIdGenerator::new();
        let id = resolve_request_id(&h, &g);

        // 结果始终满足 1–128 个可见 ASCII 字符
        prop_assert!(is_valid(id.as_str().as_bytes()));

        if vals.len() == 1 && is_valid(&vals[0]) {
            // 恰好一个合法值：原样使用
            prop_assert_eq!(id.as_str().as_bytes(), vals[0].as_slice());
        } else {
            // 否则生成新的 UUID v7，且不等于任何入站值，每次互不相同
            let u = uuid::Uuid::parse_str(id.as_str()).expect("generated id is a uuid");
            prop_assert_eq!(u.get_version_num(), 7);
            prop_assert_eq!(id.as_str().len(), 36);
            let id2 = resolve_request_id(&h, &g);
            prop_assert_ne!(id, id2);
        }
    }
}
