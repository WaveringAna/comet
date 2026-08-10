//! Peer/device trust model: pairing, stable identity, revocation.
//!
//! A peer is trusted only after an explicit pairing. Pairing is an out-of-band exchange:
//! the local device hands the remote a short pairing code, the remote proves it holds
//! its Ed25519 private key by signing a fresh challenge, and both sides record each
//! other's public key and encrypted iroh ticket.
//! Until then, a discovered peer is a *stranger*: it appears in the LAN scan but no RPC
//! is accepted from it. Revocation retains the record for inspection but makes existing
//! and future connections unusable; forgetting removes it entirely.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::identity::verify_signature;

/// Capability granted to a trusted peer. Default-trusted peers get `Peer`; fully
/// equal devices (the same user's two machines) can be elevated to `Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]

    /// Default: read-only watch methods + safe control RPCs, but never auth/credential
    /// surfaces and never destructive local-only operations.
    Peer,
    /// Full peer: every method the authz policy allows remotely.
    Admin,
}

/// A paired device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub endpoint: String,
    pub role: Role,
    /// Ed25519 public key used to verify a fresh nonce signature on every connection.
    pub public_key: String,
    pub paired_at: chrono::DateTime<chrono::Utc>,
    pub revoked: bool,
}

/// A pairing code: 6 digits, short-lived, single-use, exchanged out of band.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub code: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    failed_attempts: u8,
}

impl Pairing {
    pub const CODE_LEN: usize = 6;
    const TTL_SECS: i64 = 300;
    const MAX_FAILED_ATTEMPTS: u8 = 5;

    pub fn new() -> Self {
        Self {
            code: random_code(Self::CODE_LEN),
            created_at: chrono::Utc::now(),
            failed_attempts: 0,
        }
    }

    pub fn is_expired(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        now.signed_duration_since(self.created_at).num_seconds() > Self::TTL_SECS
    }
}

/// Process-lifetime pairing gate shared by the local settings RPC and Nova listener.
/// A valid code is consumed atomically, so two callers cannot reuse it.
#[derive(Default)]
pub struct PairingState(Mutex<Option<Pairing>>);

impl PairingState {
    pub fn begin(&self) -> Pairing {
        let pairing = Pairing::new();
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(pairing.clone());
        pairing
    }

    pub fn cancel(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub fn consume(&self, code: &str) -> bool {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(pairing) = slot.as_mut() else {
            return false;
        };
        if pairing.is_expired(chrono::Utc::now()) {
            *slot = None;
            return false;
        }
        if pairing.code != code {
            pairing.failed_attempts += 1;
            if pairing.failed_attempts >= Pairing::MAX_FAILED_ATTEMPTS {
                *slot = None;
            }
            return false;
        }
        *slot = None;
        true
    }
}

impl Default for Pairing {
    fn default() -> Self {
        Self::new()
    }
}

/// Persistable trust store: the set of peers this device has paired with.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TrustStore {
    peers: HashMap<String, TrustedPeer>,
}

impl TrustStore {
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        if path.exists()
            && let Ok(s) = serde_json::from_slice::<TrustStore>(&std::fs::read(path)?)
        {
            return Ok(s);
        }
        Ok(Self::default())
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            use std::os::unix::fs::PermissionsExt as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(&bytes)?;
            drop(file);
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, bytes)
        }
    }

    /// Pair a peer after its public-key ownership was verified by the transport.
    pub fn pair(&mut self, peer: TrustedPeer) {
        self.peers.insert(peer.device_id.clone(), peer);
    }

    /// Revoke a peer: kept in the store but marked revoked so an inbound connection
    /// is refused. Use `forget` to drop it entirely.
    pub fn revoke(&mut self, device_id: &str) -> bool {
        if let Some(p) = self.peers.get_mut(device_id) {
            p.revoked = true;
            true
        } else {
            false
        }
    }

    pub fn reinstate(&mut self, device_id: &str) -> bool {
        if let Some(p) = self.peers.get_mut(device_id) {
            p.revoked = false;
            true
        } else {
            false
        }
    }

    pub fn forget(&mut self, device_id: &str) -> bool {
        self.peers.remove(device_id).is_some()
    }

    pub fn update(&mut self, device_id: &str, name: String, endpoint: String, role: Role) -> bool {
        let Some(peer) = self.peers.get_mut(device_id) else {
            return false;
        };
        peer.name = name;
        peer.endpoint = endpoint;
        peer.role = role;
        true
    }

    pub fn get(&self, device_id: &str) -> Option<&TrustedPeer> {
        self.peers.get(device_id)
    }

    /// A live (paired + not revoked) peer.
    pub fn is_trusted(&self, device_id: &str) -> bool {
        self.peers.get(device_id).is_some_and(|p| !p.revoked)
    }

    /// Verify a fresh inbound signature against the public key captured at pairing.
    pub fn verify_signature(
        &self,
        device_id: &str,
        public_key: &str,
        message: &[u8],
        signature: &str,
    ) -> Option<Role> {
        let peer = self.peers.get(device_id)?;
        if peer.revoked {
            return None;
        }
        if peer.public_key == public_key && verify_signature(public_key, message, signature) {
            Some(peer.role)
        } else {
            None
        }
    }

    pub fn live_peers(&self) -> Vec<&TrustedPeer> {
        self.peers.values().filter(|p| !p.revoked).collect()
    }

    pub fn all_peers(&self) -> Vec<&TrustedPeer> {
        self.peers.values().collect()
    }
}

/// Thread-safe in-memory wrapper for the trust store, as the engine sees it.
pub struct Trust {
    store: Mutex<TrustStore>,
    path: std::path::PathBuf,
    changed: watch::Sender<u64>,
}

impl Trust {
    pub fn load(path: std::path::PathBuf) -> Self {
        #[cfg(unix)]
        if path.exists() {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let store = TrustStore::load_or_create(&path).unwrap_or_default();
        let (changed, _) = watch::channel(0);
        Self {
            store: Mutex::new(store),
            path,
            changed,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TrustStore> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn pair(&self, peer: TrustedPeer) -> std::io::Result<()> {
        self.lock().pair(peer);
        self.persist()?;
        self.notify_changed();
        Ok(())
    }

    pub fn revoke(&self, id: &str) -> std::io::Result<bool> {
        let changed = self.lock().revoke(id);
        if changed {
            self.persist()?;
            self.notify_changed();
        }
        Ok(changed)
    }

    pub fn forget(&self, id: &str) -> std::io::Result<bool> {
        let changed = self.lock().forget(id);
        if changed {
            self.persist()?;
            self.notify_changed();
        }
        Ok(changed)
    }

    pub fn update(
        &self,
        id: &str,
        name: String,
        endpoint: String,
        role: Role,
    ) -> std::io::Result<bool> {
        let changed = self.lock().update(id, name, endpoint, role);
        if changed {
            self.persist()?;
            self.notify_changed();
        }
        Ok(changed)
    }

    pub fn peer(&self, id: &str) -> Option<TrustedPeer> {
        self.lock().get(id).cloned()
    }

    pub fn verify_signature(
        &self,
        id: &str,
        public_key: &str,
        message: &[u8],
        signature: &str,
    ) -> Option<Role> {
        self.lock()
            .verify_signature(id, public_key, message, signature)
    }

    pub fn is_trusted(&self, id: &str) -> bool {
        self.lock().is_trusted(id)
    }

    pub fn snapshot(&self) -> TrustStore {
        // Cheap-ish clone for UI reads; stores are small (paired devices only).
        let g = self.lock();
        let mut out = TrustStore::default();
        for p in g.all_peers() {
            out.pair(p.clone());
        }
        out
    }

    /// Change notification for peer lists and the background sync loop. The
    /// value itself is only a generation counter; consumers always re-read the
    /// authoritative persisted trust snapshot.
    pub fn watch_changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn notify_changed(&self) {
        self.changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }

    fn persist(&self) -> std::io::Result<()> {
        let g = self.lock();
        g.save(&self.path)
    }
}

fn random_code(len: usize) -> String {
    // 6-digit numeric pairing code from OS RNG via uuid v4 bytes.
    let uid = uuid::Uuid::new_v4();
    let bytes = uid.as_bytes();
    let mut out = String::with_capacity(len);
    for i in 0..len {
        out.push((b'0' + (bytes[i % bytes.len()] % 10)) as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceSecret;

    fn peer(_id: &str, role: Role) -> (TrustedPeer, DeviceSecret) {
        let s = DeviceSecret::generate();
        let public_key = s.public_key().unwrap();
        let id = s.device_id();
        (
            TrustedPeer {
                device_id: id.clone(),
                name: id,
                platform: "test".into(),
                endpoint: "nova-iroh:test-ticket".into(),
                role,
                public_key,
                paired_at: chrono::Utc::now(),
                revoked: false,
            },
            s,
        )
    }

    #[test]
    fn pairing_and_verify() {
        let (p, _s) = peer("aaaa", Role::Peer);
        let mut store = TrustStore::default();
        store.pair(p);
        let peer = store.live_peers()[0];
        assert!(store.is_trusted(&peer.device_id));
    }

    #[test]
    fn revocation_blocks_verify() {
        let (p, _s) = peer("bbbb", Role::Admin);
        let mut store = TrustStore::default();
        let id = p.device_id.clone();
        store.pair(p);
        assert!(store.revoke(&id));
        assert!(!store.is_trusted(&id));
        // Reinstate restores access.
        assert!(store.reinstate(&id));
        assert!(store.is_trusted(&id));
    }

    #[test]
    fn forget_drops_entry() {
        let (p, _s) = peer("cccc", Role::Peer);
        let mut store = TrustStore::default();
        let id = p.device_id.clone();
        store.pair(p);
        assert!(store.forget(&id));
        assert!(!store.is_trusted(&id));
    }

    #[test]
    fn pairing_code_expires() {
        let p = Pairing::new();
        assert_eq!(p.code.len(), 6);
        assert!(p.code.chars().all(|c| c.is_ascii_digit()));
        let later = chrono::Utc::now() + chrono::Duration::seconds(301);
        assert!(p.is_expired(later));
    }

    #[test]
    fn trust_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        {
            let trust = Trust::load(path.clone());
            let (p, _s) = peer("dddd", Role::Peer);
            let id = p.device_id.clone();
            trust.pair(p).unwrap();
            assert!(trust.is_trusted(&id));
        }
        // Reload from disk in a new instance.
        let trust = Trust::load(path.clone());
        let id = trust.snapshot().live_peers()[0].device_id.clone();
        assert!(trust.is_trusted(&id));
        trust.revoke(&id).unwrap();
        let trust = Trust::load(path);
        assert!(!trust.is_trusted(&id));
    }

    #[test]
    fn pairing_code_is_single_use() {
        let state = PairingState::default();
        let pairing = state.begin();
        assert!(state.consume(&pairing.code));
        assert!(!state.consume(&pairing.code));
    }

    #[test]
    fn pairing_code_closes_after_repeated_bad_guesses() {
        let state = PairingState::default();
        let pairing = state.begin();
        let wrong = if pairing.code == "000000" {
            "111111"
        } else {
            "000000"
        };
        for _ in 0..Pairing::MAX_FAILED_ATTEMPTS {
            assert!(!state.consume(wrong));
        }
        assert!(!state.consume(&pairing.code));
    }
}
