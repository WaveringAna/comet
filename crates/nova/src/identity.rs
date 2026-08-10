//! Stable device identity for Nova Engine.
//!
//! A device owns an Ed25519 signing seed stored mode 0600. Peers retain only the public
//! key and verify a fresh nonce signature on every connection, so there is no reusable
//! bearer attestation to copy from a trust store or network capture.

use std::path::Path;

use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use sha2::{Digest, Sha256};

/// 32-byte long-term device secret.
#[derive(Clone)]
pub struct DeviceSecret(pub [u8; 32]);

impl DeviceSecret {
    /// Generate a fresh random secret from the operating system CSPRNG.
    pub fn generate() -> Self {
        let mut out = [0u8; 32];
        SystemRandom::new()
            .fill(&mut out)
            .expect("operating system random source unavailable");
        Self(out)
    }

    fn key_pair(&self) -> Option<Ed25519KeyPair> {
        Ed25519KeyPair::from_seed_unchecked(&self.0).ok()
    }

    pub fn public_key(&self) -> Option<String> {
        self.key_pair().map(|key| hex(key.public_key().as_ref()))
    }

    /// Public device id: 12 hex chars of SHA-256(public key).
    pub fn device_id(&self) -> String {
        let public_key = self
            .key_pair()
            .map(|key| key.public_key().as_ref().to_vec())
            .unwrap_or_else(|| self.0.to_vec());
        let mut h = Sha256::new();
        h.update(public_key);
        let digest = h.finalize();
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }

    pub fn sign(&self, message: &[u8]) -> Option<String> {
        self.key_pair().map(|key| hex(key.sign(message).as_ref()))
    }
}

pub fn device_id_for_public_key(public_key: &str) -> Option<String> {
    let bytes = decode_hex_vec(public_key)?;
    if bytes.len() != 32 {
        return None;
    }
    let digest = Sha256::digest(bytes);
    Some(digest.iter().take(6).map(|b| format!("{b:02x}")).collect())
}

pub fn verify_signature(public_key: &str, message: &[u8], signature: &str) -> bool {
    let Some(public_key) = decode_hex_vec(public_key) else {
        return false;
    };
    let Some(signature) = decode_hex_vec(signature) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message, &signature)
        .is_ok()
}

/// A persistable identity: device id + the secret bytes (stored mode 0600).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeviceIdentityRecord {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub secret: String, // hex
}

impl DeviceIdentityRecord {
    /// Load or create the identity file at `path`. Created with a random name the UI
    /// can rename later.
    pub fn load_or_create(path: &Path, platform: &str) -> std::io::Result<Self> {
        Self::load_or_create_inner(path, platform, None)
    }

    /// Load/create the signing identity while retaining the engine's existing stable
    /// device id. Nova authentication binds that id into every signature; it does not
    /// require replacing workspace ids when a signing key is first introduced.
    pub fn load_or_create_for_device(
        path: &Path,
        platform: &str,
        device_id: &str,
    ) -> std::io::Result<Self> {
        Self::load_or_create_inner(path, platform, Some(device_id))
    }

    fn load_or_create_inner(
        path: &Path,
        platform: &str,
        device_id: Option<&str>,
    ) -> std::io::Result<Self> {
        if path.exists() {
            let bytes = std::fs::read(path)?;
            if let Ok(mut record) = serde_json::from_slice::<DeviceIdentityRecord>(&bytes)
                && let Some(secret) = record.secret()
            {
                let expected = device_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| secret.device_id());
                if record.device_id != expected {
                    record.device_id = expected;
                    save_secret(path, &record)?;
                }
                tighten_secret_permissions(path)?;
                return Ok(record);
            }
            // Fall through and regenerate on corruption.
        }
        let secret = DeviceSecret::generate();
        let record = Self {
            device_id: device_id
                .map(str::to_owned)
                .unwrap_or_else(|| secret.device_id()),
            name: hostname(),
            platform: platform.to_string(),
            secret: hex(&secret.0),
        };
        save_secret(path, &record)?;
        Ok(record)
    }

    pub fn secret(&self) -> Option<DeviceSecret> {
        let mut out = [0u8; 32];
        if decode_hex(&self.secret, &mut out) {
            Some(DeviceSecret(out))
        } else {
            None
        }
    }

    pub fn public_key(&self) -> Option<String> {
        self.secret()?.public_key()
    }

    pub fn sign(&self, message: &[u8]) -> Option<String> {
        self.secret()?.sign(message)
    }
}

#[cfg(unix)]
fn save_secret(path: &Path, record: &DeviceIdentityRecord) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let bytes = serde_json::to_vec_pretty(record)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    drop(file);
    tighten_secret_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn tighten_secret_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn save_secret(path: &Path, record: &DeviceIdentityRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, serde_json::to_vec_pretty(record)?)
}

#[cfg(not(unix))]
fn tighten_secret_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .or_else(command_hostname)
        .unwrap_or_else(|| "Nova device".to_string())
}

fn command_hostname() -> Option<String> {
    let output = std::process::Command::new("hostname").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode_hex(s: &str, out: &mut [u8]) -> bool {
    let s = s.trim();
    if s.len() != out.len() * 2 {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[i * 2]);
        let lo = hex_val(bytes[i * 2 + 1]);
        match (hi, lo) {
            (Some(h), Some(l)) => *slot = (h << 4) | l,
            _ => return false,
        }
    }
    true
}

pub fn decode_hex_vec(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = vec![0; s.len() / 2];
    decode_hex(s, &mut out).then_some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_fresh_messages() {
        let secret = DeviceSecret::generate();
        let public = secret.public_key().unwrap();
        let signature = secret.sign(b"fresh nonce").unwrap();
        assert!(verify_signature(&public, b"fresh nonce", &signature));
        assert!(!verify_signature(&public, b"other nonce", &signature));
        assert_eq!(device_id_for_public_key(&public), Some(secret.device_id()));
    }

    #[test]
    fn corrupt_secret_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "device_id": "old",
                "name": "test",
                "platform": "test",
                "secret": "not-a-secret"
            })
            .to_string(),
        )
        .unwrap();
        let record = DeviceIdentityRecord::load_or_create_for_device(&path, "test", "stable")
            .expect("corrupt identity should regenerate");
        assert_eq!(record.device_id, "stable");
        assert!(record.secret().is_some());
    }
}
