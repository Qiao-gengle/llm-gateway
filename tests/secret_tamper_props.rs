//! Property 2: 篡改检测
//!
//! **Validates: Requirements 13.5**

use std::sync::Arc;

use llm_gateway::clock::OsRandom;
use llm_gateway::secret::{DecryptError, SecretStore};
use proptest::prelude::*;

fn store(key: [u8; 32]) -> SecretStore {
    SecretStore::new(&key, Arc::new(OsRandom))
}

/// 错误的 Display 不含明文的任何长度 ≥ 4 的子串。
/// 若明文本身恰好含有类别名的子串（如 "Auth"），该用例无意义，由调用方跳过。
fn display_overlaps(plain: &[u8], display: &str) -> bool {
    plain.windows(4).any(|w| {
        std::str::from_utf8(w)
            .map(|s| display.contains(s))
            .unwrap_or(false)
    })
}

fn check_err(plain: &[u8], err: DecryptError) -> Result<(), TestCaseError> {
    prop_assert_eq!(err, DecryptError::AuthenticationFailed);
    let shown = err.to_string();
    prop_assert!(!display_overlaps(plain, &shown));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn p02_tamper_detection(
        plain in proptest::collection::vec(any::<u8>(), 0..=256),
        key in any::<[u8; 32]>(),
        other_key in any::<[u8; 32]>(),
        nonce_bit in 0usize..96,
        ct_bit_seed in any::<usize>(),
    ) {
        prop_assume!(key != other_key);
        let probe = DecryptError::AuthenticationFailed.to_string();
        prop_assume!(!display_overlaps(&plain, &probe));

        let s = store(key);
        let sealed = s.seal(&plain);
        // 未篡改时可解密（排除非 UTF-8 明文：open 返回 String）
        if std::str::from_utf8(&plain).is_ok() {
            prop_assert!(s.open(&sealed).is_ok());
        }

        // 翻转 nonce 中任意一位
        let mut t = sealed.clone();
        t.nonce[nonce_bit / 8] ^= 1 << (nonce_bit % 8);
        check_err(&plain, s.open(&t).unwrap_err())?;

        // 翻转 ciphertext（含 tag）中任意一位
        let bit = ct_bit_seed % (sealed.ciphertext.len() * 8);
        let mut t = sealed.clone();
        t.ciphertext[bit / 8] ^= 1 << (bit % 8);
        check_err(&plain, s.open(&t).unwrap_err())?;

        // 另一个 Master_Key
        check_err(&plain, store(other_key).open(&sealed).unwrap_err())?;
    }
}
