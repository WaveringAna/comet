//! Direct workspace and transcript convergence between paired Nova Engines.
//!
//! The hosted Loro rooms used to be both a rendezvous service and a CRDT relay.
//! Pairing now gives us authenticated ordinary RPC links, so synchronization is
//! a small two-call exchange over that same connection:
//!
//! 1. fetch the peer's workspace/chat version-vector heads;
//! 2. send the updates it lacks plus our pre-exchange heads, then import the
//!    updates it returns relative to those heads.
//!
//! Loro remains the local document model, but there is no room server, bearer,
//! organization, or hosted persistence in this path.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{StreamExt as _, stream};
use loro::VersionVector;
use serde::{Deserialize, Serialize};

use crate::nova::NovaHost;
use crate::{DocHost, EngineError, WorkspaceHost};

const SYNC_INTERVAL: Duration = Duration::from_secs(5);
const PEER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CHAT_DOCS: usize = 4_096;
const MAX_UPDATE_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOTAL_UPDATE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHeads {
    pub workspace: String,
    pub chats: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncDoc {
    /// The sender's version before applying the other side's update.
    pub version: String,
    /// Loro update containing everything the receiver's advertised head lacked.
    pub update: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncExchange {
    pub workspace: SyncDoc,
    pub chats: BTreeMap<String, SyncDoc>,
}

#[derive(Clone)]
pub struct PeerSync {
    nova: NovaHost,
    workspace: WorkspaceHost,
    docs: DocHost,
}

impl PeerSync {
    pub fn new(nova: NovaHost, workspace: WorkspaceHost, docs: DocHost) -> Self {
        Self {
            nova,
            workspace,
            docs,
        }
    }

    pub fn heads(&self) -> Result<SyncHeads, EngineError> {
        let chat_ids = self.docs.sync_chat_ids()?;
        if chat_ids.len() > MAX_CHAT_DOCS {
            return Err(EngineError::Other(format!(
                "peer sync has too many chats ({} > {MAX_CHAT_DOCS})",
                chat_ids.len()
            )));
        }
        let mut chats = BTreeMap::new();
        for chat_id in chat_ids {
            chats.insert(chat_id.clone(), encode(self.docs.sync_version(&chat_id)?));
        }
        Ok(SyncHeads {
            workspace: encode(self.workspace.sync_version()),
            chats,
        })
    }

    /// Import a peer's deltas and return everything it lacked at the versions
    /// it supplied. Workspace lands first so host ownership is known before an
    /// imported chat command can wake its executor.
    pub fn apply(&self, exchange: SyncExchange) -> Result<SyncExchange, EngineError> {
        if exchange.chats.len() > MAX_CHAT_DOCS {
            return Err(EngineError::Other(format!(
                "peer sync request has too many chats ({} > {MAX_CHAT_DOCS})",
                exchange.chats.len()
            )));
        }
        let mut decoded_total = 0usize;
        let workspace_version = decode_version(&exchange.workspace.version)?;
        let workspace_update = decode_update(&exchange.workspace.update, &mut decoded_total)?;
        self.workspace.sync_import(&workspace_update)?;
        let workspace_reply = SyncDoc {
            version: encode(self.workspace.sync_version()),
            update: encode(self.workspace.sync_export(&workspace_version)?),
        };

        let mut chats = BTreeMap::new();
        for (chat_id, request) in exchange.chats {
            validate_chat_id(&chat_id)?;
            let version = decode_version(&request.version)?;
            let update = decode_update(&request.update, &mut decoded_total)?;
            self.docs.sync_import(&chat_id, &update)?;
            chats.insert(
                chat_id.clone(),
                SyncDoc {
                    version: encode(self.docs.sync_version(&chat_id)?),
                    update: encode(self.docs.sync_export(&chat_id, &version)?),
                },
            );
        }
        Ok(SyncExchange {
            workspace: workspace_reply,
            chats,
        })
    }

    pub async fn sync_peer(&self, device_id: &str) -> Result<(), EngineError> {
        let client = self
            .nova
            .dial(device_id)
            .await
            .map_err(|error| EngineError::Other(error.to_string()))?;
        let remote: SyncHeads = serde_json::from_value(
            client
                .call(
                    comet_nova::methods::NOVA_SYNC_HEADS,
                    serde_json::Value::Null,
                )
                .await
                .map_err(|error| EngineError::Other(error.to_string()))?,
        )
        .map_err(|error| EngineError::Other(format!("peer sync heads decode failed: {error}")))?;

        let request = self.exchange_for(&remote)?;
        let response: SyncExchange = serde_json::from_value(
            client
                .call(
                    comet_nova::methods::NOVA_SYNC_APPLY,
                    serde_json::to_value(request).map_err(|error| {
                        EngineError::Other(format!("peer sync request encode failed: {error}"))
                    })?,
                )
                .await
                .map_err(|error| EngineError::Other(error.to_string()))?,
        )
        .map_err(|error| {
            EngineError::Other(format!("peer sync response decode failed: {error}"))
        })?;
        self.import_response(response)?;
        self.workspace.note_peer_seen(device_id);
        Ok(())
    }

    fn exchange_for(&self, remote: &SyncHeads) -> Result<SyncExchange, EngineError> {
        if remote.chats.len() > MAX_CHAT_DOCS {
            return Err(EngineError::Other(format!(
                "peer advertised too many chats ({} > {MAX_CHAT_DOCS})",
                remote.chats.len()
            )));
        }
        let local = self.heads()?;
        let mut ids: BTreeSet<String> = local.chats.keys().cloned().collect();
        ids.extend(remote.chats.keys().cloned());
        if ids.len() > MAX_CHAT_DOCS {
            return Err(EngineError::Other(format!(
                "peer sync union has too many chats ({} > {MAX_CHAT_DOCS})",
                ids.len()
            )));
        }

        let workspace_remote = decode_version(&remote.workspace)?;
        let workspace = SyncDoc {
            version: local.workspace,
            update: encode(self.workspace.sync_export(&workspace_remote)?),
        };
        let empty = VersionVector::default().encode();
        let mut chats = BTreeMap::new();
        for chat_id in ids {
            validate_chat_id(&chat_id)?;
            let remote_version = match remote.chats.get(&chat_id) {
                Some(version) => decode_version(version)?,
                None => empty.clone(),
            };
            let local_version = local
                .chats
                .get(&chat_id)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| self.docs.sync_version(&chat_id).map(encode))?;
            chats.insert(
                chat_id.clone(),
                SyncDoc {
                    version: local_version,
                    update: encode(self.docs.sync_export(&chat_id, &remote_version)?),
                },
            );
        }
        Ok(SyncExchange { workspace, chats })
    }

    fn import_response(&self, response: SyncExchange) -> Result<(), EngineError> {
        if response.chats.len() > MAX_CHAT_DOCS {
            return Err(EngineError::Other(
                "peer sync response has too many chats".into(),
            ));
        }
        let mut decoded_total = 0usize;
        self.workspace.sync_import(&decode_update(
            &response.workspace.update,
            &mut decoded_total,
        )?)?;
        for (chat_id, response) in response.chats {
            validate_chat_id(&chat_id)?;
            let update = decode_update(&response.update, &mut decoded_total)?;
            self.docs.sync_import(&chat_id, &update)?;
        }
        Ok(())
    }

    /// Start the process-lifetime peer convergence loop. Trust changes wake it
    /// immediately; the interval repairs missed updates and sleep/wake links.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut trust = self.nova.watch_trust();
            let mut tick = tokio::time::interval(SYNC_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    changed = trust.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
                let peers = self.nova.live_peer_ids();
                let this = self.clone();
                stream::iter(peers)
                    .map(|device_id| {
                        let this = this.clone();
                        async move {
                            let result =
                                tokio::time::timeout(PEER_TIMEOUT, this.sync_peer(&device_id))
                                    .await;
                            match result {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => {
                                    tracing::debug!(%device_id, %error, "nova peer sync failed");
                                    this.nova.invalidate(&device_id).await;
                                }
                                Err(_) => {
                                    tracing::debug!(%device_id, "nova peer sync timed out");
                                    this.nova.invalidate(&device_id).await;
                                }
                            }
                        }
                    })
                    .buffer_unordered(4)
                    .collect::<Vec<_>>()
                    .await;
            }
        })
    }
}

fn encode(bytes: Vec<u8>) -> String {
    BASE64.encode(bytes)
}

fn decode_version(encoded: &str) -> Result<Vec<u8>, EngineError> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|error| EngineError::Other(format!("invalid peer sync version: {error}")))?;
    VersionVector::decode(&bytes)
        .map_err(|error| EngineError::Other(format!("invalid peer sync version: {error}")))?;
    Ok(bytes)
}

fn decode_update(encoded: &str, total: &mut usize) -> Result<Vec<u8>, EngineError> {
    let bytes = BASE64
        .decode(encoded)
        .map_err(|error| EngineError::Other(format!("invalid peer sync update: {error}")))?;
    if bytes.len() > MAX_UPDATE_BYTES {
        return Err(EngineError::Other(
            "peer sync update exceeds the per-document limit".into(),
        ));
    }
    *total = total.saturating_add(bytes.len());
    if *total > MAX_TOTAL_UPDATE_BYTES {
        return Err(EngineError::Other(
            "peer sync exchange exceeds the total limit".into(),
        ));
    }
    Ok(bytes)
}

fn validate_chat_id(chat_id: &str) -> Result<(), EngineError> {
    if chat_id.is_empty() || chat_id.len() > 256 || chat_id.chars().any(char::is_control) {
        return Err(EngineError::Other("invalid chat id in peer sync".into()));
    }
    Ok(())
}
