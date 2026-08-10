//! comet-sync — local SQLite document snapshots and the processed-command ledger.
//!
//! Direct CRDT convergence lives in `comet-engine::peer_sync` because it is
//! coupled to paired Nova identities and the engine's workspace/doc hosts.

mod store;

pub use store::{DocsStore, StoreError};
