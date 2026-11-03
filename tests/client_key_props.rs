//! Client_Key 生成格式的属性测试。

use llm_gateway::secret::{base62_decode_32, client_key_from_bytes, sha256_key};
use proptest::prelude::*;
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};

fn cases() -> u32 {
    let env = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    env.max(256)
}

fn is_base62(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    // Feature: llm-gateway, Property 11: Client_Key 生成格式
    /// **Validates: Requirements 11.1**
    #[test]
    fn p11_client_key_format(r in proptest::array::uniform32(any::<u8>())) {
        let g = client_key_from_bytes(&r);
        let k = g.plaintext.expose_secret();

        prop_assert_eq!(k.len(), 6 + 43);
        prop_assert!(k.starts_with("sk-gw-"));
        let suffix = &k[6..];
        prop_assert_eq!(suffix.len(), 43);
        prop_assert!(is_base62(suffix));

        prop_assert_eq!(base62_decode_32(suffix), Some(r));
        prop_assert_eq!(&g.display_prefix[..], &k[..12]);

        let expected: [u8; 32] = Sha256::digest(k.as_bytes()).into();
        prop_assert_eq!(g.hash, expected);
        prop_assert_eq!(g.hash, sha256_key(k));
    }
}

#[test]
fn p11_edge_inputs_are_left_padded() {
    let g = client_key_from_bytes(&[0u8; 32]);
    assert_eq!(g.plaintext.expose_secret(), &format!("sk-gw-{}", "0".repeat(43)));
    let g = client_key_from_bytes(&[0xff; 32]);
    assert_eq!(g.plaintext.expose_secret().len(), 49);
}
