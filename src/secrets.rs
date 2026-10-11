//! Encryption at rest for saved logins, wallet cards, and connector secrets.
//!
//! XChaCha20-Poly1305 with a random 32-byte key stored in
//! `<data_dir>/vault.key` (mode 0600, created on first use). Each record is
//! `version (1 byte) || nonce (24 bytes) || ciphertext+tag`, and the record
//! id is bound as associated data so ciphertexts cannot be swapped between
//! rows. Secrets are decrypted only in memory, only when needed, and are
//! never returned by the HTTP API or logged.

use crate::config::write_private_file;
use crate::util::{Result, random_bytes};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::path::Path;

const VERSION: u8 = 1;
const NONCE_LEN: usize = 24;

#[derive(Clone)]
pub struct Vault {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Vault(..)")
    }
}

impl Vault {
    /// Load the key file, creating it (0600) if missing. Refuses keys that
    /// are readable by group or others.
    pub fn open(key_file: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let key: [u8; 32] = match std::fs::read(key_file) {
            Ok(bytes) => {
                let mode = std::fs::metadata(key_file)?.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(format!(
                        "{} must not be accessible by group/others (chmod 600)",
                        key_file.display()
                    )
                    .into());
                }
                bytes
                    .try_into()
                    .map_err(|_| format!("{} is not a 32-byte key", key_file.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = random_bytes::<32>();
                write_private_file(key_file, &key)?;
                key
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self::from_key(&key))
    }

    pub fn from_key(key: &[u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new_from_slice(key).expect("32-byte key"),
        }
    }

    pub fn encrypt(&self, plaintext: &[u8], aad: &str) -> Result<Vec<u8>> {
        let nonce_bytes = random_bytes::<NONCE_LEN>();
        let nonce = XNonce::from(nonce_bytes);
        let ct = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "encryption failed")?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn decrypt(&self, record: &[u8], aad: &str) -> Result<Vec<u8>> {
        if record.len() < 1 + NONCE_LEN + 16 || record[0] != VERSION {
            return Err("malformed vault record".into());
        }
        let nonce_bytes: [u8; NONCE_LEN] = record[1..1 + NONCE_LEN].try_into().expect("len");
        let nonce = XNonce::from(nonce_bytes);
        self.cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &record[1 + NONCE_LEN..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "vault record failed authentication".into())
    }

    /// Encrypt a string to base64 (for JSON-embedded connector secrets).
    pub fn seal_str(&self, plaintext: &str, aad: &str) -> Result<String> {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD
            .encode(self.encrypt(plaintext.as_bytes(), aad)?))
    }

    pub fn open_str(&self, sealed: &str, aad: &str) -> Result<String> {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.decode(sealed)?;
        Ok(String::from_utf8(self.decrypt(&raw, aad)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper_detection() {
        let v = Vault::from_key(&[7u8; 32]);
        let ct = v.encrypt(b"hunter2", "cred-1").unwrap();
        assert_eq!(v.decrypt(&ct, "cred-1").unwrap(), b"hunter2");
        // Bound to the record id.
        assert!(v.decrypt(&ct, "cred-2").is_err());
        let mut bad = ct.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(v.decrypt(&bad, "cred-1").is_err());
        // Fresh nonce every time.
        assert_ne!(v.encrypt(b"x", "a").unwrap(), v.encrypt(b"x", "a").unwrap());
        assert!(!String::from_utf8_lossy(&ct).contains("hunter2"));
        let sealed = v.seal_str("token", "conn:x").unwrap();
        assert_eq!(v.open_str(&sealed, "conn:x").unwrap(), "token");
    }

    #[test]
    fn key_file_created_private_and_reused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.key");
        let a = Vault::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let ct = a.encrypt(b"s", "id").unwrap();
        let b = Vault::open(&path).unwrap();
        assert_eq!(b.decrypt(&ct, "id").unwrap(), b"s");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Vault::open(&path).is_err());
    }
}
