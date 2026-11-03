//! Request_ID 解析、生成与中间件（需求 35）。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// 请求标识：1–128 个 0x21–0x7E 字符。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub String);

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 生成 UUID v7 字符串（36 个可见 ASCII 字符）。
#[derive(Clone, Debug, Default)]
pub struct RequestIdGenerator;

impl RequestIdGenerator {
    pub fn new() -> Self {
        Self
    }

    pub fn generate(&self) -> RequestId {
        RequestId(uuid::Uuid::now_v7().to_string())
    }
}

fn is_valid(v: &[u8]) -> bool {
    (1..=128).contains(&v.len()) && v.iter().all(|b| (0x21..=0x7E).contains(b))
}

/// 恰好一个合法 `x-request-id` 头则原样使用，否则生成新值。
pub fn resolve_request_id(headers: &HeaderMap, gen_: &RequestIdGenerator) -> RequestId {
    let mut it = headers.get_all(REQUEST_ID_HEADER).iter();
    if let (Some(v), None) = (it.next(), it.next()) {
        if is_valid(v.as_bytes()) {
            // 合法字节均为 ASCII 可见字符，转换不会失败。
            if let Ok(s) = std::str::from_utf8(v.as_bytes()) {
                return RequestId(s.to_owned());
            }
        }
    }
    gen_.generate()
}

/// axum 中间件：放入请求 extensions，并在响应上 `insert` 唯一的 `x-request-id`。
pub async fn request_id_middleware(
    State(gen_): State<Arc<RequestIdGenerator>>,
    mut req: Request,
    next: Next,
) -> Response {
    let rid = resolve_request_id(req.headers(), &gen_);
    req.extensions_mut().insert(rid.clone());
    let mut resp = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(rid.as_str()) {
        resp.headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(vals: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in vals {
            h.append(REQUEST_ID_HEADER, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn keeps_single_valid() {
        let g = RequestIdGenerator::new();
        assert_eq!(resolve_request_id(&hm(&["abc-1"]), &g).as_str(), "abc-1");
    }

    #[test]
    fn regenerates_on_invalid() {
        let g = RequestIdGenerator::new();
        for h in [hm(&[]), hm(&["a", "b"]), hm(&["a b"]), hm(&[&"x".repeat(129)])] {
            let id = resolve_request_id(&h, &g);
            assert_eq!(id.as_str().len(), 36);
        }
        assert_eq!(resolve_request_id(&hm(&[&"x".repeat(128)]), &g).as_str().len(), 128);
    }

    #[test]
    fn generated_unique() {
        let g = RequestIdGenerator::new();
        let set: std::collections::HashSet<_> = (0..10_000).map(|_| g.generate()).collect();
        assert_eq!(set.len(), 10_000);
    }
}
