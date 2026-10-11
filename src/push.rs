//! Web Push (RFC 8030) with VAPID (RFC 8292) and `aes128gcm` message
//! encryption (RFC 8291), implemented on RustCrypto primitives so no
//! heavyweight push library is needed.
//!
//! The VAPID key pair is generated on first use and stored in
//! `<data_dir>/vapid.json` (0600). Payloads are small JSON objects:
//! `{"title", "body", "conversation_id", "message_id", "kind"}`.

use crate::config::write_private_file;
use crate::util::{Result, random_bytes};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::path::Path;

const RECORD_SIZE: u32 = 4096;

#[derive(Clone)]
pub struct VapidKeys {
    secret: SecretKey,
    /// Uncompressed SEC1 public key, base64url (the browser's
    /// `applicationServerKey`).
    pub public_key_b64: String,
}

#[derive(Serialize, Deserialize)]
struct VapidFile {
    private_key: String,
    public_key: String,
}

fn random_secret_key() -> SecretKey {
    loop {
        // A random 32-byte string is a valid scalar with overwhelming
        // probability; retry on the astronomically rare failure.
        if let Ok(k) = SecretKey::from_slice(&random_bytes::<32>()) {
            return k;
        }
    }
}

fn public_bytes(secret: &SecretKey) -> Vec<u8> {
    secret.public_key().to_sec1_bytes().to_vec()
}

impl VapidKeys {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let f: VapidFile = serde_json::from_str(&text)?;
                let secret = SecretKey::from_slice(&B64URL.decode(f.private_key)?)
                    .map_err(|_| "invalid VAPID private key")?;
                Ok(Self {
                    public_key_b64: B64URL.encode(public_bytes(&secret)),
                    secret,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = random_secret_key();
                let keys = Self {
                    public_key_b64: B64URL.encode(public_bytes(&secret)),
                    secret,
                };
                let file = VapidFile {
                    private_key: B64URL.encode(keys.secret.to_bytes()),
                    public_key: keys.public_key_b64.clone(),
                };
                write_private_file(path, serde_json::to_string_pretty(&file)?.as_bytes())?;
                Ok(keys)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `Authorization` header value for a push service endpoint.
    pub fn authorization(&self, endpoint: &str, subject: &str, now_secs: i64) -> Result<String> {
        let aud = origin(endpoint).ok_or("push endpoint must be an https URL")?;
        let header = B64URL.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let mut claims = serde_json::json!({"aud": aud, "exp": now_secs + 12 * 3600});
        if !subject.is_empty() {
            claims["sub"] = subject.into();
        }
        let claims = B64URL.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let key = SigningKey::from(&self.secret);
        let sig: Signature = key.sign(signing_input.as_bytes());
        let jwt = format!("{signing_input}.{}", B64URL.encode(sig.to_bytes()));
        Ok(format!("vapid t={jwt}, k={}", self.public_key_b64))
    }
}

/// `https://host[:port]` of an endpoint URL.
fn origin(endpoint: &str) -> Option<String> {
    let rest = endpoint.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("https://{host}"))
}

/// Encrypt `payload` for a subscription (RFC 8291, single record).
pub fn encrypt(payload: &[u8], p256dh_b64: &str, auth_b64: &str) -> Result<Vec<u8>> {
    let ua_public_bytes = B64URL.decode(p256dh_b64.trim_end_matches('='))?;
    let auth_secret = B64URL.decode(auth_b64.trim_end_matches('='))?;
    let ua_public =
        PublicKey::from_sec1_bytes(&ua_public_bytes).map_err(|_| "invalid p256dh key")?;
    if auth_secret.len() != 16 {
        return Err("auth secret must be 16 bytes".into());
    }
    if payload.len() + 17 + 86 > RECORD_SIZE as usize {
        return Err("push payload too large".into());
    }
    let as_secret = random_secret_key();
    let as_public = public_bytes(&as_secret);
    let salt = random_bytes::<16>();
    let ciphertext = encrypt_with(
        payload,
        &ua_public_bytes,
        &ua_public,
        &auth_secret,
        &as_secret,
        &as_public,
        &salt,
    )?;
    let mut body = Vec::with_capacity(86 + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Derive the content-encryption key and nonce (RFC 8291 section 3.4).
fn derive(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12])> {
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), ecdh_secret)
        .expand(&key_info, &mut ikm)
        .map_err(|_| "hkdf")?;
    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| "hkdf")?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| "hkdf")?;
    Ok((cek, nonce))
}

fn encrypt_with(
    payload: &[u8],
    ua_public_bytes: &[u8],
    ua_public: &PublicKey,
    auth_secret: &[u8],
    as_secret: &SecretKey,
    as_public: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>> {
    let shared = as_secret.diffie_hellman(ua_public);
    let (cek, nonce) = derive(
        shared.raw_secret_bytes().as_slice(),
        auth_secret,
        ua_public_bytes,
        as_public,
        salt,
    )?;
    let mut plaintext = payload.to_vec();
    plaintext.push(0x02); // last-record delimiter, no padding
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| "aes key")?;
    cipher
        .encrypt(&Nonce::from(nonce), plaintext.as_slice())
        .map_err(|_| "aes-gcm encryption failed".into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendResult {
    Delivered,
    /// 404/410: the subscription is gone and should be deleted.
    Expired,
    Failed(u16),
}

/// Deliver one encrypted notification (blocking; call from spawn_blocking).
pub fn send_blocking(
    keys: &VapidKeys,
    subject: &str,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    payload: &serde_json::Value,
) -> Result<SendResult> {
    let body = encrypt(payload.to_string().as_bytes(), p256dh, auth)?;
    let authorization = keys.authorization(endpoint, subject, crate::util::now_ms() / 1000)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build()
        .into();
    let resp = agent
        .post(endpoint)
        .header("TTL", "86400")
        .header("Urgency", "normal")
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("Authorization", &authorization)
        .send(&body[..])?;
    let status = resp.status().as_u16();
    Ok(match status {
        200..=299 => SendResult::Delivered,
        404 | 410 => SendResult::Expired,
        s => SendResult::Failed(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    /// Receiver side of RFC 8291, used to check our sender.
    fn decrypt(body: &[u8], ua_secret: &SecretKey, auth: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
        assert_eq!(rs, RECORD_SIZE);
        let idlen = body[20] as usize;
        let as_public = &body[21..21 + idlen];
        let ct = &body[21 + idlen..];
        let shared = ua_secret.diffie_hellman(&PublicKey::from_sec1_bytes(as_public).unwrap());
        let ua_public = public_bytes(ua_secret);
        let (cek, nonce) = derive(
            shared.raw_secret_bytes().as_slice(),
            auth,
            &ua_public,
            as_public,
            salt,
        )
        .unwrap();
        let mut pt = Aes128Gcm::new_from_slice(&cek)
            .unwrap()
            .decrypt(&Nonce::from(nonce), ct)
            .unwrap();
        assert_eq!(pt.pop(), Some(0x02));
        pt
    }

    #[test]
    fn encrypts_for_subscription() {
        let ua = random_secret_key();
        let auth = random_bytes::<16>();
        let p256dh = B64URL.encode(public_bytes(&ua));
        assert_eq!(public_bytes(&ua).len(), 65, "uncompressed point");
        let body = encrypt(br#"{"title":"hi"}"#, &p256dh, &B64URL.encode(auth)).unwrap();
        assert_eq!(decrypt(&body, &ua, &auth), br#"{"title":"hi"}"#);
        assert!(encrypt(b"x", &p256dh, &B64URL.encode([0u8; 3])).is_err());
        assert!(encrypt(&[b'a'; 5000], &p256dh, &B64URL.encode(auth)).is_err());
    }

    /// RFC 8291 Appendix A test vector.
    #[test]
    fn matches_rfc8291_vector() {
        let d = |s: &str| B64URL.decode(s).unwrap();
        let as_secret =
            SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        let as_public = public_bytes(&as_secret);
        assert_eq!(
            B64URL.encode(&as_public),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let ua_public_bytes = d(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let ua_public = PublicKey::from_sec1_bytes(&ua_public_bytes).unwrap();
        let salt = d("DGv6ra1nlYgDCS1FRnbzlw");
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");
        let ct = encrypt_with(
            b"When I grow up, I want to be a watermelon",
            &ua_public_bytes,
            &ua_public,
            &auth,
            &as_secret,
            &as_public,
            &salt,
        )
        .unwrap();
        let mut body = salt.clone();
        body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
        body.push(65);
        body.extend_from_slice(&as_public);
        body.extend_from_slice(&ct);
        assert_eq!(
            B64URL.encode(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn vapid_jwt_verifies_and_keys_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vapid.json");
        let keys = VapidKeys::load_or_create(&path).unwrap();
        let again = VapidKeys::load_or_create(&path).unwrap();
        assert_eq!(keys.public_key_b64, again.public_key_b64);
        let h = keys
            .authorization(
                "https://push.example.com/send/abc",
                "mailto:ops@example.com",
                1000,
            )
            .unwrap();
        let t = h
            .strip_prefix("vapid t=")
            .unwrap()
            .split(", k=")
            .next()
            .unwrap();
        let parts: Vec<_> = t.split('.').collect();
        let claims: serde_json::Value =
            serde_json::from_slice(&B64URL.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.com");
        assert_eq!(claims["exp"], 1000 + 12 * 3600);
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        let vk =
            VerifyingKey::from_sec1_bytes(&B64URL.decode(&keys.public_key_b64).unwrap()).unwrap();
        let sig = Signature::from_slice(&B64URL.decode(parts[2]).unwrap()).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
        assert!(
            keys.authorization("http://insecure.example", "", 0)
                .is_err()
        );
    }
}
