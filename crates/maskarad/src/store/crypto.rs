//! AES-256-GCM sealing of stored mappings. Redis only ever sees ciphertext.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 12;

#[derive(Clone)]
pub struct Cipher {
    cipher: Aes256Gcm,
}

impl Cipher {
    /// Base64 of 32 bytes is used as is; anything else is hashed to 32 bytes.
    pub fn from_secret(secret: &str) -> Self {
        let key_bytes: [u8; 32] =
            match base64::engine::general_purpose::STANDARD.decode(secret.trim()) {
                Ok(b) if b.len() == 32 => b.try_into().expect("checked length"),
                _ => Sha256::digest(secret.as_bytes()).into(),
            };
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        Self {
            cipher: Aes256Gcm::new(key),
        }
    }

    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let mut out = nonce.to_vec();
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("AES-GCM encryption cannot fail");
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ct) = data.split_at(NONCE_LEN);
        self.cipher.decrypt(Nonce::from_slice(nonce), ct).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let c = Cipher::from_secret("passphrase");
        let sealed = c.seal(b"hello");
        assert_eq!(c.open(&sealed).unwrap(), b"hello");
        let mut bad = sealed.clone();
        bad[15] ^= 1;
        assert!(c.open(&bad).is_none());
        assert!(Cipher::from_secret("other").open(&sealed).is_none());
    }
}
