//! Secret_Store：Upstream_Key 的 AES-256-GCM 加解密（见 design.md“Secret_Store”）。

use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use secrecy::SecretString;

use crate::clock::RandomSource;

/// 密文与 nonce。`ciphertext` 末尾含 16 字节认证 tag。
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed").finish_non_exhaustive()
    }
}

/// `Display` 只输出类别名，不含任何明文或密文内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecryptError {
    #[error("BadNonceLength")]
    BadNonceLength,
    #[error("AuthenticationFailed")]
    AuthenticationFailed,
}

pub struct SecretStore {
    cipher: Aes256Gcm,
    rng: Arc<dyn RandomSource>,
}

impl SecretStore {
    pub fn new(master_key: &[u8; 32], rng: Arc<dyn RandomSource>) -> Self {
        Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key)),
            rng,
        }
    }

    /// 每次调用生成新的随机 nonce；不使用 AAD。
    pub fn seal(&self, plaintext: &[u8]) -> Sealed {
        let mut nonce = [0u8; 12];
        self.rng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .expect("AES-GCM encryption cannot fail for in-memory input");
        Sealed { nonce, ciphertext }
    }

    pub fn open(&self, sealed: &Sealed) -> Result<SecretString, DecryptError> {
        let plain = self
            .cipher
            .decrypt(Nonce::from_slice(&sealed.nonce), sealed.ciphertext.as_slice())
            .map_err(|_| DecryptError::AuthenticationFailed)?;
        String::from_utf8(plain)
            .map(SecretString::from)
            .map_err(|_| DecryptError::AuthenticationFailed)
    }
}

pub const CLIENT_KEY_PREFIX: &str = "sk-gw-";
const BASE62_LEN: usize = 43;
const DISPLAY_PREFIX_LEN: usize = 12;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 新生成的 Client_Key。明文只在创建响应中返回一次。
pub struct GeneratedKey {
    pub plaintext: SecretString,
    /// 明文前 12 个字符
    pub display_prefix: String,
    /// `sha256(plaintext)`
    pub hash: [u8; 32],
}

impl std::fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedKey")
            .field("display_prefix", &self.display_prefix)
            .finish_non_exhaustive()
    }
}

/// 32 字节大端整数 → 定宽 43 位 base62（左侧补 `0`）。
pub fn base62_encode_32(bytes: &[u8; 32]) -> String {
    let mut num = *bytes;
    let mut out = [b'0'; BASE62_LEN];
    for slot in out.iter_mut().rev() {
        let mut rem: u32 = 0;
        for b in num.iter_mut() {
            let cur = (rem << 8) | u32::from(*b);
            *b = (cur / 62) as u8;
            rem = cur % 62;
        }
        *slot = ALPHABET[rem as usize];
    }
    debug_assert!(num.iter().all(|b| *b == 0), "43 base62 digits hold 256 bits");
    String::from_utf8(out.to_vec()).expect("alphabet is ASCII")
}

/// `base62_encode_32` 的逆运算；长度不为 43、含非法字符或超出 256 位时返回 `None`。
pub fn base62_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != BASE62_LEN {
        return None;
    }
    let mut num = [0u8; 32];
    for c in s.bytes() {
        let digit = ALPHABET.iter().position(|a| *a == c)? as u32;
        let mut carry = digit;
        for b in num.iter_mut().rev() {
            let cur = u32::from(*b) * 62 + carry;
            *b = (cur & 0xff) as u8;
            carry = cur >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    Some(num)
}

pub fn sha256_key(plaintext: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(plaintext.as_bytes()).into()
}

/// 由给定的 32 字节随机数构造 Client_Key（纯函数，便于属性测试）。
pub fn client_key_from_bytes(random: &[u8; 32]) -> GeneratedKey {
    let plaintext = format!("{CLIENT_KEY_PREFIX}{}", base62_encode_32(random));
    GeneratedKey {
        display_prefix: plaintext[..DISPLAY_PREFIX_LEN].to_owned(),
        hash: sha256_key(&plaintext),
        plaintext: SecretString::from(plaintext),
    }
}

/// 生成 `sk-gw-` + 43 位 base62 的 Client_Key。`rng` 在生产环境必须是 `OsRandom`。
pub fn generate_client_key(rng: &dyn RandomSource) -> GeneratedKey {
    let mut random = [0u8; 32];
    rng.fill_bytes(&mut random);
    client_key_from_bytes(&random)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{OsRandom, SeededRandom};

    #[test]
    fn client_key_format_and_hash() {
        let k = generate_client_key(&OsRandom);
        let p = k.plaintext.expose_secret();
        assert_eq!(p.len(), 6 + 43);
        assert!(p.starts_with("sk-gw-"));
        assert!(p[6..].bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(k.display_prefix, p[..12]);
        assert_eq!(k.hash, sha256_key(p));
        let other = generate_client_key(&OsRandom);
        assert_ne!(other.plaintext.expose_secret(), p);
    }

    #[test]
    fn base62_round_trip_and_fixed_width() {
        let zero = base62_encode_32(&[0; 32]);
        assert_eq!(zero, "0".repeat(43));
        assert_eq!(base62_decode_32(&zero), Some([0; 32]));
        let max = [0xff; 32];
        let enc = base62_encode_32(&max);
        assert_eq!(enc.len(), 43);
        assert_eq!(base62_decode_32(&enc), Some(max));
        let rng = SeededRandom::new(3);
        for _ in 0..50 {
            let mut r = [0u8; 32];
            rng.fill_bytes(&mut r);
            assert_eq!(base62_decode_32(&base62_encode_32(&r)), Some(r));
        }
        assert_eq!(base62_decode_32("short"), None);
        assert_eq!(base62_decode_32(&"z".repeat(43)), None);
    }

    use secrecy::ExposeSecret;

    fn store(key: [u8; 32]) -> SecretStore {
        SecretStore::new(&key, Arc::new(OsRandom))
    }

    #[test]
    fn round_trip() {
        let s = store([7; 32]);
        let sealed = s.seal(b"sk-upstream-123");
        assert_eq!(sealed.ciphertext.len(), "sk-upstream-123".len() + 16);
        assert_eq!(s.open(&sealed).unwrap().expose_secret(), "sk-upstream-123");
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let s = store([1; 32]);
        assert_eq!(s.open(&s.seal(b"")).unwrap().expose_secret(), "");
    }

    #[test]
    fn nonces_differ_between_calls() {
        let s = store([7; 32]);
        assert_ne!(s.seal(b"x").nonce, s.seal(b"x").nonce);
        let seeded = SecretStore::new(&[7; 32], Arc::new(SeededRandom::new(1)));
        assert_ne!(seeded.seal(b"x").nonce, seeded.seal(b"x").nonce);
    }

    #[test]
    fn tampering_and_wrong_key_fail() {
        let s = store([7; 32]);
        let sealed = s.seal(b"secret-value");

        let mut a = sealed.clone();
        a.nonce[0] ^= 1;
        assert_eq!(s.open(&a).unwrap_err(), DecryptError::AuthenticationFailed);

        let mut b = sealed.clone();
        b.ciphertext[0] ^= 0x80;
        assert_eq!(s.open(&b).unwrap_err(), DecryptError::AuthenticationFailed);

        let mut c = sealed.clone();
        c.ciphertext.truncate(3);
        assert_eq!(s.open(&c).unwrap_err(), DecryptError::AuthenticationFailed);

        let err = store([8; 32]).open(&sealed).unwrap_err();
        assert_eq!(err, DecryptError::AuthenticationFailed);
        assert_eq!(err.to_string(), "AuthenticationFailed");
    }
}
