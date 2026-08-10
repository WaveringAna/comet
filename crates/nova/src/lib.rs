//! comet-nova — the Nova Engine peer networking layer.
//!
//! Nova Engine is the device-local backend (formerly "the daemon"/"the engine"). This
//! crate adds the transport that connects Nova Engines without a hosted application
//! backend. Iroh may relay end-to-end encrypted packets when direct hole punching fails:
//!
//! - [`cidr] — CIDR parsing/normalization for user-entered scan ranges.
//! - [`identity`] — stable per-device identity + ed25519 signing key.
//! - [`trust`] — pairing, roles, revocation.
//! - [`authz`] — method-level authorization (IPC-only surfaces stay local).
//! - [`discovery`] — bounded, cancellable, deduplicated LAN scanning.
//! - [`transport`] — encrypted, mutually authenticated iroh QUIC RPC on top of
//!   the transport-independent `comet-rpc` seam.
//!
//! Workspace and transcript convergence also run over authenticated Nova RPC; no hosted
//! room or application-level relay can read or mutate them. See `docs/ARCHITECTURE-Nova.md`.

pub mod authz;
pub mod cidr;
pub mod discovery;
pub mod identity;
pub mod transport;
pub mod trust;

/// Nova Engine RPC method names (served by the engine, surfaced in Settings).
///
/// These are separate from comet-rpc's harness/chat surface: they configure the peer
/// network itself. They are always IPC-only (never relay-forwarded) — the settings UI
/// talks to the local engine.
pub mod methods {
    /// Generate a fresh pairing code for this device. Returns `{code, expiresAt}`.
    pub const NOVA_BEGIN_PAIRING: &str = "NovaBeginPairing";
    /// Cancel an in-flight pairing.
    pub const NOVA_CANCEL_PAIRING: &str = "NovaCancelPairing";
    /// Pair to an iroh ticket with its short code (`{endpoint, code}`).
    pub const NOVA_PAIR_PEER: &str = "NovaPairPeer";
    /// Update a paired peer's display name, endpoint, or role.
    pub const NOVA_UPDATE_PEER: &str = "NovaUpdatePeer";
    /// Prove a paired peer is reachable over the direct transport.
    pub const NOVA_TEST_PEER: &str = "NovaTestPeer";
    /// Revoke a paired peer.
    pub const NOVA_REVOKE_PEER: &str = "NovaRevokePeer";
    /// Forget a paired peer entirely.
    pub const NOVA_FORGET_PEER: &str = "NovaForgetPeer";
    /// List paired peers.
    pub const NOVA_LIST_PEERS: &str = "NovaListPeers";
    /// Stream the paired-peer list, including trust changes accepted inbound.
    pub const NOVA_WATCH_PEERS: &str = "NovaWatchPeers";
    /// Run a LAN scan. Params: `{ranges: ["10.0.0.0/8", ...], port?}`; streams
    /// [`crate::discovery::FoundPeer`] items, then `{done: true}`.
    pub const NOVA_SCAN: &str = "NovaScan";
    /// This device's id + name (IPC-only).
    pub const NOVA_LOCAL_DEVICE: &str = "NovaLocalDevice";
    /// Internal peer-sync handshake: return workspace/chat Loro version heads.
    pub const NOVA_SYNC_HEADS: &str = "NovaSyncHeads";
    /// Internal peer-sync exchange: import caller deltas and return responder deltas.
    pub const NOVA_SYNC_APPLY: &str = "NovaSyncApply";
}
