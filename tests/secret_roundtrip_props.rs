//! Property 1: Upstream_Key 加解密往返
//!
//! **Validates: Requirements 13.4, 13.2**

use std::sync::Arc;

use llm_gateway::clock::OsRandom;
use llm_gateway::secret::SecretStore;
use proptest::prelude::*;
use secrecy::ExposeSecret;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// `open` 返回 `SecretString`，因此明文限定为合法 UTF-8；
    /// 任意 Unicode 字符串（至多 1024 个字符，即至多 4096 字节）。
    #[test]
    fn p01_seal_open_round_trip(
        key in prop::array::uniform32(any::<u8>()),
        plain in "\\PC{0,1024}",
    ) {
        prop_assert!(plain.len() <= 4096);
        let store = SecretStore::new(&key, Arc::new(OsRandom));
        let s1 = store.seal(plain.as_bytes());
        let s2 = store.seal(plain.as_bytes());

        let opened = store.open(&s1).unwrap();
        prop_assert_eq!(opened.expose_secret().as_bytes(), plain.as_bytes());
        let opened2 = store.open(&s2).unwrap();
        prop_assert_eq!(opened2.expose_secret().as_bytes(), plain.as_bytes());

        prop_assert_ne!(s1.nonce, s2.nonce);
        prop_assert_ne!(s1.ciphertext, s2.ciphertext);
    }

    /// 单字节（ASCII）明文边界到 4096 字节的长度覆盖。
    #[test]
    fn p01_ascii_lengths_up_to_4096(
        key in prop::array::uniform32(any::<u8>()),
        plain in "[\\x00-\\x7f]{0,4096}",
    ) {
        let store = SecretStore::new(&key, Arc::new(OsRandom));
        let s1 = store.seal(plain.as_bytes());
        let s2 = store.seal(plain.as_bytes());
        prop_assert_eq!(s1.ciphertext.len(), plain.len() + 16);
        let opened = store.open(&s1).unwrap();
        prop_assert_eq!(opened.expose_secret().as_bytes(), plain.as_bytes());
        prop_assert_ne!(s1.nonce, s2.nonce);
        prop_assert_ne!(s1.ciphertext, s2.ciphertext);
    }
}
