//! EngineRpc — the engine-side `RpcService`: sessions + docs + the workspace-doc
//! entity surface.
//!
//! Methods (feature-inventory §2):
//! - `ListHarnesses` → `[HarnessDescriptor]`
//! - `ListModels {harness}` → `[Model]`
//! - `QueueCommand {chatId, command}` → `{commandId}` (durable doc command)
//! - `WatchDocMessages {chatId}` → stream of joined `SessionMessageEntry[]`,
//!   re-emitted on every doc change
//! - `WatchChats` / `WatchDevices` → streams of the workspace doc's entity rows
//! - `WatchSessions` → stream of `Session[]`: this engine's live statuses merged with
//!   remote devices' workspace session rows
//! - `Mutate {op, …}` → `{ok}` — workspace entity mutations (createChat, renameChat,
//!   setChatArchived, deleteChat, renameDevice, markChatSeen)
//! - `LocalDevice` → `{deviceId}` — this engine's identity (never forwarded)
//! - Repos (§3.5): `ListRepos`, `AddRepo {path}`, `CloneRepo {url}`,
//!   `CreateRepo {name}`, `ListBranches {repoPath}` (default branch first),
//!   `ListFolders {path?}`, `CreateWorktree {repoPath, branch}`, `DeleteWorktree
//!   {repoPath, worktreePath}`; `WatchCheckoutDiffs` → stream of `CheckoutDiff[]`
//! - Terminals (§3.4): `OpenTerminal {chatId, cols, rows}` → `TerminalSession`,
//!   `SubscribeTerminal {terminalId, afterSeq?}` → stream of `TerminalEvent`
//!   (replay then live tail), `WriteTerminal {terminalId, data}`, `ResizeTerminal`,
//!   `CloseTerminal`. Terminal access is authorized at the Nova peer boundary;
//!   sessions themselves remain device-local.
//! - Agent accounts (§3.7): `ListAgentAccounts {forceUsage?}` →
//!   `AgentAccountsSnapshot`, `ActivateAgentAccount`/`ForgetAgentAccount`
//!   `{harness, accountId}` → snapshot, `StartAgentLogin {harness}` →
//!   `{loginId, url, mode}`, `CompleteAgentLogin {loginId, code}` → snapshot,
//!   `PollAgentLogin {loginId}`, `CancelAgentLogin {loginId}`.
//! - Uploads (§3.7): `UploadChunk {uploadId, data, seq?}`,
//!   `UploadCommit {uploadId, fileName}` → `{path}`,
//!   `ReadAttachmentChunk {path, offset}` → `{name, mimeType, data, nextOffset,
//!   done}` (path-jailed to the uploads dir + workspace-known chat cwds).
//!
//! ## Device-addressed routing (`targetDeviceId`, feature-inventory §2.1)
//!
//! ControlRpc methods are Nova-forwardable: params may carry `targetDeviceId`. When it
//! names another device, the call is forwarded verbatim over its authenticated direct
//! connection. The remote engine sees its own id and handles locally, so the forward
//! can never loop. Streaming methods are proxied by re-subscribing remotely and
//! piping items. To make another method device-addressable, nothing per-method is needed
//! beyond listing it in [`forwardable`] (and [`is_stream_method`] if it streams);
//! handlers stay transport-agnostic. Currently routed: `ListHarnesses`, `ListModels`,
//! `QueueCommand`, and `WatchDocMessages`.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use tokio::sync::watch;

use comet_doc::SessionCommandPayload;
use comet_proto::{ChatConfig, CollaborationControlRequest, HarnessId, PiSettingsScope};
use comet_rpc::{RpcError, RpcReply, RpcService, methods, parse_params};

use crate::agent_accounts::AgentAccounts;
use crate::diff_sync::CheckoutDiffSync;
use crate::doc_host::DocHost;
use crate::nova::NovaHost;
use crate::peer_sync::{PeerSync, SyncExchange};
use crate::pi_management::PiManagement;
use crate::registry::HarnessRegistry;
use crate::repos::{Repos, home_dir};
use crate::sessions::SessionsEngine;
use crate::terminals::Terminals;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatParams {
    chat_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListModelsParams {
    harness: HarnessId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPathParams {
    /// `repoPath` per §3.5 (the §2.1 shorthand `repo` is accepted as an alias).
    #[serde(alias = "repo")]
    repo_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchRefParams {
    /// The checkout to switch — a session's cwd (main folder or worktree).
    repo_path: String,
    ref_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    branch: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    #[serde(alias = "path")]
    worktree_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListFoldersParams {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolOutputParams {
    chat_id: String,
    tool_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenTerminalParams {
    chat_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalIdParams {
    terminal_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeTerminalParams {
    terminal_id: String,
    #[serde(default)]
    after_seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteTerminalParams {
    terminal_id: String,
    /// Base64 input bytes (plain UTF-8 accepted leniently).
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeTerminalParams {
    terminal_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAgentAccountsParams {
    #[serde(default)]
    force_usage: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentAccountParams {
    harness: HarnessId,
    account_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAgentLoginParams {
    harness: HarnessId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginIdParams {
    login_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteAgentLoginParams {
    login_id: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiSettingsParams {
    #[serde(default = "global_pi_scope")]
    scope: PiSettingsScope,
    #[serde(default)]
    project_path: Option<String>,
}

fn global_pi_scope() -> PiSettingsScope {
    PiSettingsScope::Global
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetPiSettingParams {
    #[serde(default = "global_pi_scope")]
    scope: PiSettingsScope,
    #[serde(default)]
    project_path: Option<String>,
    key: String,
    value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetPiCredentialParams {
    #[serde(default = "global_pi_scope")]
    scope: PiSettingsScope,
    #[serde(default)]
    project_path: Option<String>,
    provider: String,
    key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemovePiCredentialParams {
    #[serde(default = "global_pi_scope")]
    scope: PiSettingsScope,
    #[serde(default)]
    project_path: Option<String>,
    provider: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetPiOpenAiCompatibleParams {
    #[serde(default = "global_pi_scope")]
    scope: PiSettingsScope,
    #[serde(default)]
    project_path: Option<String>,
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PiPackageActionParams {
    #[serde(default = "global_pi_scope")]
    scope: PiSettingsScope,
    #[serde(default)]
    project_path: Option<String>,
    action: String,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadChunkParams {
    upload_id: String,
    /// Base64 payload chunk.
    data: String,
    #[serde(default)]
    seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadCommitParams {
    upload_id: String,
    file_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadAttachmentChunkParams {
    path: String,
    #[serde(default)]
    offset: u64,
}

/// The Mutate surface (feature-inventory §2 DataRpc), tagged by `op`.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum MutateParams {
    #[serde(rename_all = "camelCase")]
    CreateChat {
        chat_id: String,
        /// The project the chat is created in — fixes host device + base cwd.
        project_id: String,
        #[serde(default)]
        config: Option<ChatConfig>,
        /// The picked ref, named on the row from the first frame (the footer
        /// read "Select ref" until the diff reconciler stamped it).
        #[serde(default)]
        branch: Option<String>,
        /// Cwd override (isolated-worktree path); default = the project's folder.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Create a project (device + folder pair). Idempotent by id; a live
    /// duplicate `(deviceId, path)` no-ops. `gitDetected` is seeded from the
    /// picker's FolderEntry — the owning device's ProjectsSync re-verifies.
    #[serde(rename_all = "camelCase")]
    CreateProject {
        project_id: String,
        device_id: String,
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        git_detected: bool,
    },
    /// LWW display-name set; `name: None` clears back to basename(path).
    #[serde(rename_all = "camelCase")]
    RenameProject {
        project_id: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Hard delete: cascades to every chat (and session row) in the project.
    /// Live runs hosted here are interrupted best-effort.
    #[serde(rename_all = "camelCase")]
    DeleteProject { project_id: String },
    #[serde(rename_all = "camelCase")]
    RenameChat { chat_id: String, title: String },
    /// Set the chat's checkout branch label — the sidebar's
    /// "project · branch" sub-line.
    #[serde(rename_all = "camelCase")]
    SetChatBranch { chat_id: String, branch: String },
    /// Retarget a chat onto another folder — mid-session switch to an
    /// EXISTING worktree (the picked ref's checkout). Next run starts a
    /// fresh harness conversation there (resume is cwd-scoped).
    #[serde(rename_all = "camelCase")]
    SetChatCwd { chat_id: String, cwd: String },
    /// Backdate a chat's activity timestamps (epoch ms) — the sidebar's
    /// relative-time column. Used by tooling/seeds; the doc fold sets these on
    /// real message traffic.
    #[serde(rename_all = "camelCase")]
    SetChatActivity {
        chat_id: String,
        #[serde(default)]
        last_message_at: Option<i64>,
        #[serde(default)]
        created_at: Option<i64>,
    },
    /// Re-home a chat to another device (tooling/seeds; device migration later).
    #[serde(rename_all = "camelCase")]
    SetChatHost { chat_id: String, device_id: String },
    #[serde(rename_all = "camelCase")]
    SetChatArchived { chat_id: String, archived: bool },
    /// Full-config replace on the chat row (comet `SetChatConfig`): the
    /// composer's mid-session model / reasoning / options changes, LWW-synced
    /// so they survive restarts and reach every device.
    #[serde(rename_all = "camelCase")]
    SetChatConfig { chat_id: String, config: ChatConfig },
    /// Tombstone: removes the chats-map row; the session doc remains.
    #[serde(rename_all = "camelCase")]
    DeleteChat { chat_id: String },
    #[serde(rename_all = "camelCase")]
    RenameDevice { device_id: String, name: String },
    /// Synced seen marker (LWW + monotonic guard): clears the "completed"
    /// badge on every device. `at` is epoch ms; default = now.
    #[serde(rename_all = "camelCase")]
    MarkChatSeen {
        chat_id: String,
        #[serde(default)]
        at: Option<i64>,
    },
}

pub struct EngineRpc {
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    registry: std::sync::Arc<HarnessRegistry>,
    repos: Repos,
    terminals: Terminals,
    diff_sync: CheckoutDiffSync,
    uploads: Uploads,
    agent_accounts: AgentAccounts,
    pi_management: PiManagement,
    updater: Option<comet_update::Updater>,
    nova: Option<NovaHost>,
}

impl EngineRpc {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
        registry: std::sync::Arc<HarnessRegistry>,
        repos: Repos,
        terminals: Terminals,
        diff_sync: CheckoutDiffSync,
        uploads: Uploads,
        agent_accounts: AgentAccounts,
    ) -> Self {
        Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            uploads,
            agent_accounts,
            pi_management: PiManagement,
            updater: None,
            nova: None,
        }
    }

    /// Attach the release checker (UpdateStatus stream + ApplyUpdate).
    pub fn with_updater(mut self, updater: comet_update::Updater) -> Self {
        self.updater = Some(updater);
        self
    }

    /// Attach the Nova Engine peer host (Nova* settings RPCs + inbound peer auth).
    pub fn with_nova(mut self, nova: NovaHost) -> Self {
        self.nova = Some(nova);
        self
    }

    fn nova(&self) -> Result<&NovaHost, RpcError> {
        self.nova.as_ref().ok_or_else(|| {
            RpcError::Failed("Nova peer networking is not enabled on this engine".into())
        })
    }

    fn updater(&self) -> Result<&comet_update::Updater, RpcError> {
        self.updater
            .as_ref()
            .ok_or_else(|| RpcError::Failed("updates unavailable".into()))
    }

    /// Forward a device-addressed call over its paired direct Nova connection.
    async fn forward(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let nova = self.nova()?.clone();
        let client = nova.dial(target).await?;
        if is_stream_method(method) {
            let rx = match client.subscribe(method, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    nova.invalidate(target).await;
                    return Err(err);
                }
            };
            // Pipe remote items; the held client keeps the link's RpcClient alive for
            // the stream's lifetime. A remote error just ends the stream (the direct
            // link-down path fails pending calls; stream receivers close).
            let stream = futures::stream::unfold((rx, client), |(mut rx, client)| async move {
                rx.recv().await.map(|item| (item, (rx, client)))
            });
            return Ok(RpcReply::Stream(stream.boxed()));
        }
        match client.call(method, params).await {
            Ok(value) => Ok(RpcReply::Value(value)),
            Err(err) => {
                if matches!(err, RpcError::Closed | RpcError::Transport(_)) {
                    nova.invalidate(target).await;
                }
                Err(err)
            }
        }
    }

    fn mutate(&self, params: MutateParams) -> Result<(), RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match params {
            MutateParams::CreateChat {
                chat_id,
                project_id,
                config,
                branch,
                cwd,
            } => {
                self.workspace
                    .create_chat(&chat_id, &project_id, config, cwd)
                    .map_err(failed)?;
                if let Some(branch) = branch.as_deref().filter(|b| !b.is_empty()) {
                    self.workspace
                        .set_chat_branch(&chat_id, branch)
                        .map_err(failed)?;
                }
                Ok(())
            }
            MutateParams::CreateProject {
                project_id,
                device_id,
                path,
                name,
                git_detected,
            } => self
                .workspace
                .create_project(&project_id, &device_id, &path, name, git_detected)
                .map_err(failed),
            MutateParams::RenameProject { project_id, name } => self
                .workspace
                .rename_project(&project_id, name.as_deref())
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteProject { project_id } => {
                let deleted = self.workspace.delete_project(&project_id).map_err(failed)?;
                // Best-effort teardown of live runs we host for the deleted chats
                // (the doc rows are already tombstoned; a straggler run would only
                // write into an orphaned session doc).
                let sessions = self.sessions.clone();
                let chat_ids = deleted.chat_ids;
                tokio::spawn(async move {
                    for chat_id in chat_ids {
                        if let Err(err) = sessions.interrupt(&chat_id).await {
                            tracing::debug!(chat = %chat_id, error = %err, "deleteProject interrupt skipped");
                        }
                    }
                });
                Ok(())
            }
            MutateParams::RenameChat { chat_id, title } => self
                .workspace
                .rename_chat(&chat_id, &title)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatBranch { chat_id, branch } => self
                .workspace
                .set_chat_branch(&chat_id, &branch)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatCwd { chat_id, cwd } => self
                .workspace
                .set_chat_cwd(&chat_id, &cwd)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatActivity {
                chat_id,
                last_message_at,
                created_at,
            } => self
                .workspace
                .set_chat_activity(&chat_id, last_message_at, created_at)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatHost { chat_id, device_id } => self
                .workspace
                .set_chat_host(&chat_id, &device_id)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatArchived { chat_id, archived } => self
                .workspace
                .set_chat_archived(&chat_id, archived)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatConfig { chat_id, config } => self
                .workspace
                .set_chat_config(&chat_id, &config)
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteChat { chat_id } => self
                .workspace
                .delete_chat(&chat_id)
                .map_err(failed)
                .map(drop),
            MutateParams::RenameDevice { device_id, name } => self
                .workspace
                .rename_device(&device_id, &name)
                .map_err(failed)
                .map(drop),
            MutateParams::MarkChatSeen { chat_id, at } => {
                let at = at
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                    .unwrap_or_else(chrono::Utc::now);
                self.workspace
                    .mark_chat_seen(&chat_id, at)
                    .map_err(failed)
                    .map(drop)
            }
        }
    }
}

/// ControlRpc methods that honor `targetDeviceId` (feature-inventory §2.1). Extend this
/// list (plus [`is_stream_method`] for streams) to make more of the surface
/// device-addressable — the handlers themselves need no changes.
fn forwardable(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_HARNESSES
            | methods::LIST_MODELS
            | methods::QUEUE_COMMAND
            | methods::WATCH_DOC_MESSAGES
            | methods::WATCH_COLLABORATIONS
            | methods::COLLABORATION_CONTROL
            // Repos/worktrees/folders are device-local filesystem state.
            | methods::LIST_REPOS
            | methods::ADD_REPO
            | methods::CLONE_REPO
            | methods::CREATE_REPO
            | methods::LIST_BRANCHES
            | methods::LIST_REFS
            | methods::SWITCH_REF
            | methods::LIST_FOLDERS
            | methods::CREATE_WORKTREE
            | methods::DELETE_WORKTREE
            // Checkout diffs are produced on the device holding the checkout.
            | methods::WATCH_CHECKOUT_DIFFS
            // Terminals live on the chat's host device.
            | methods::OPEN_TERMINAL
            | methods::SUBSCRIBE_TERMINAL
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL
            // Tool outputs live in the chat host's run journal (host-local,
            // never synced — that's the point of the lookup).
            | methods::TOOL_OUTPUT
            | methods::TOOL_DIFF
            // Agent accounts are per-device CLI logins (the device switcher
            // retargets which device's logins are shown).
            | methods::LIST_AGENT_ACCOUNTS
            | methods::ACTIVATE_AGENT_ACCOUNT
            | methods::FORGET_AGENT_ACCOUNT
            | methods::START_AGENT_LOGIN
            | methods::COMPLETE_AGENT_LOGIN
            | methods::POLL_AGENT_LOGIN
            | methods::CANCEL_AGENT_LOGIN
            // Pi configuration, credentials, and packages are device-local.
            | methods::GET_PI_SETTINGS
            | methods::SET_PI_SETTING
            | methods::SET_PI_CREDENTIAL
            | methods::REMOVE_PI_CREDENTIAL
            | methods::SET_PI_OPENAI_COMPATIBLE
            | methods::PI_PACKAGE_ACTION
            // Uploads/attachments target the chat's host device (the agent reads
            // the committed file from that device's disk).
            | methods::UPLOAD_CHUNK
            | methods::UPLOAD_COMMIT
            | methods::READ_ATTACHMENT_CHUNK
            // Updates report/apply on the device whose binary they concern.
            | methods::UPDATE_STATUS
            | methods::APPLY_UPDATE
    )
}

/// Forwardable methods whose reply is a stream (proxied item-by-item).
fn is_stream_method(method: &str) -> bool {
    matches!(
        method,
        methods::WATCH_DOC_MESSAGES
            | methods::WATCH_COLLABORATIONS
            | methods::SUBSCRIBE_TERMINAL
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::UPDATE_STATUS
    )
}

/// A watch receiver as a stream: current value first, then every change.
fn watch_stream<T>(rx: watch::Receiver<T>) -> BoxStream<'static, serde_json::Value>
where
    T: serde::Serialize + Clone + Send + Sync + 'static,
{
    futures::stream::unfold((rx, false), |(mut rx, emitted)| async move {
        if emitted {
            rx.changed().await.ok()?;
        }
        let value = {
            let borrowed = rx.borrow_and_update();
            serde_json::to_value(&*borrowed).ok()?
        };
        Some((value, (rx, true)))
    })
    .boxed()
}

fn nova_peer_watch_stream(nova: NovaHost) -> BoxStream<'static, serde_json::Value> {
    futures::stream::unfold(
        (nova.watch_trust(), nova, false),
        |(mut rx, nova, emitted)| async move {
            if emitted {
                rx.changed().await.ok()?;
            }
            let value = serde_json::to_value(nova.list_peers()).ok()?;
            Some((value, (rx, nova, true)))
        },
    )
    .boxed()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdParams {
    device_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairPeerParams {
    endpoint: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanParams {
    ranges: Vec<String>,
    #[serde(default = "default_nova_port")]
    port: u16,
    #[serde(default)]
    allow_public: bool,
}

fn default_nova_port() -> u16 {
    27655
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePeerParams {
    device_id: String,
    name: String,
    endpoint: String,
    #[serde(default = "default_role")]
    role: comet_nova::trust::Role,
}

fn default_role() -> comet_nova::trust::Role {
    comet_nova::trust::Role::Peer
}

#[async_trait]
impl RpcService for EngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        // Device-addressed routing: forward calls that target another paired Nova.
        if forwardable(method)
            && let Some(target) = params.get("targetDeviceId").and_then(|v| v.as_str())
            && target != self.doc_host.device_id()
        {
            let target = target.to_string();
            return self.forward(&target, method, params).await;
        }
        match method {
            methods::LIST_HARNESSES => RpcReply::value(&self.registry.descriptors()),
            methods::LIST_MODELS => {
                let p: ListModelsParams = parse_params(params)?;
                if p.harness == HarnessId::Pi
                    && let Err(error) = self.pi_management.refresh_openai_compatible().await
                {
                    tracing::warn!(%error, "OpenAI-compatible model registry refresh failed; using last saved registry");
                }
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let models = harness
                    .models()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&models)
            }
            methods::QUEUE_COMMAND => {
                let p: QueueCommandParams = parse_params(params)?;
                let command_id = self
                    .doc_host
                    .queue_command(&p.chat_id, p.command)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "commandId": command_id }))
            }
            methods::WATCH_DOC_MESSAGES => {
                let p: ChatParams = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(RpcReply::Stream(watch_stream(handle.watch_messages())))
            }
            methods::WATCH_CHATS => {
                Ok(RpcReply::Stream(watch_stream(self.workspace.watch_chats())))
            }
            methods::WATCH_DEVICES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_devices(),
            ))),
            methods::WATCH_PROJECTS => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_projects(),
            ))),
            methods::WATCH_SESSIONS => {
                // Local live statuses merged with remote devices' workspace rows.
                let merged = self
                    .workspace
                    .merged_sessions_watch(self.sessions.watch_sessions());
                Ok(RpcReply::Stream(watch_stream(merged)))
            }
            methods::WATCH_COLLABORATIONS => Ok(RpcReply::Stream(watch_stream(
                self.sessions.watch_collaborations(),
            ))),
            methods::COLLABORATION_CONTROL => {
                let request: CollaborationControlRequest = parse_params(params)?;
                let reply = self
                    .sessions
                    .collaboration_control(request)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&reply)
            }
            methods::LOCAL_DEVICE => {
                RpcReply::value(&serde_json::json!({ "deviceId": self.doc_host.device_id() }))
            }
            methods::UPDATE_STATUS => Ok(RpcReply::Stream(watch_stream(self.updater()?.watch()))),
            methods::APPLY_UPDATE => {
                let version = self
                    .updater()?
                    .apply()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true, "version": version }))
            }
            methods::MUTATE => {
                let p: MutateParams = parse_params(params)?;
                self.mutate(p)?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::WATCH_CHECKOUT_DIFFS => {
                Ok(RpcReply::Stream(watch_stream(self.diff_sync.watch_diffs())))
            }
            methods::LIST_REPOS => RpcReply::value(&self.repos.list().await),
            methods::ADD_REPO => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .add(&p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CLONE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .clone_repo(&p.url)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CREATE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .create(&p.name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::LIST_BRANCHES => {
                let p: RepoPathParams = parse_params(params)?;
                let branches = self
                    .repos
                    .branches(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&branches)
            }
            methods::LIST_REFS => {
                let p: RepoPathParams = parse_params(params)?;
                let refs = self
                    .repos
                    .refs(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&refs)
            }
            methods::SWITCH_REF => {
                let p: SwitchRefParams = parse_params(params)?;
                let branch = self
                    .repos
                    .switch_ref(std::path::Path::new(&p.repo_path), &p.ref_name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "branch": branch }))
            }
            methods::LIST_FOLDERS => {
                let p: ListFoldersParams = parse_params(params)?;
                let listing = self
                    .repos
                    .list_folders(p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&listing)
            }
            methods::CREATE_WORKTREE => {
                let p: CreateWorktreeParams = parse_params(params)?;
                let worktree = self
                    .repos
                    .create_worktree(std::path::Path::new(&p.repo_path), &p.branch)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&worktree)
            }
            methods::DELETE_WORKTREE => {
                let p: DeleteWorktreeParams = parse_params(params)?;
                self.repos
                    .delete_worktree(
                        std::path::Path::new(&p.repo_path),
                        std::path::Path::new(&p.worktree_path),
                    )
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::OPEN_TERMINAL => {
                let p: OpenTerminalParams = parse_params(params)?;
                // The terminal runs in the chat's checkout; a chat with no cwd (or
                // no row yet) gets the home directory.
                let cwd = self
                    .workspace
                    .doc()
                    .chat(&p.chat_id)
                    .ok()
                    .flatten()
                    .and_then(|chat| chat.cwd)
                    .unwrap_or_else(|| home_dir().to_string_lossy().to_string());
                let session = self
                    .terminals
                    .open(&cwd, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&session)
            }
            methods::TOOL_OUTPUT => {
                let p: ToolOutputParams = parse_params(params)?;
                let reply = self
                    .sessions
                    .journal_tool_output(&p.chat_id, &p.tool_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&reply)
            }
            methods::TOOL_DIFF => {
                let p: ToolOutputParams = parse_params(params)?;
                let reply = self
                    .sessions
                    .journal_tool_diff(&p.chat_id, &p.tool_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&reply)
            }
            methods::SUBSCRIBE_TERMINAL => {
                let p: SubscribeTerminalParams = parse_params(params)?;
                let rx = self
                    .terminals
                    .subscribe(&p.terminal_id, p.after_seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    let event = rx.recv().await?;
                    let value = serde_json::to_value(&event).ok()?;
                    Some((value, rx))
                });
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::WRITE_TERMINAL => {
                let p: WriteTerminalParams = parse_params(params)?;
                self.terminals
                    .write(&p.terminal_id, &p.data)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RESIZE_TERMINAL => {
                let p: ResizeTerminalParams = parse_params(params)?;
                self.terminals
                    .resize(&p.terminal_id, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::CLOSE_TERMINAL => {
                let p: TerminalIdParams = parse_params(params)?;
                self.terminals
                    .close(&p.terminal_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_AGENT_ACCOUNTS => {
                let p: ListAgentAccountsParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .list(p.force_usage.unwrap_or(false))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::ACTIVATE_AGENT_ACCOUNT => {
                let p: AgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .activate(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::FORGET_AGENT_ACCOUNT => {
                let p: AgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .forget(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::START_AGENT_LOGIN => {
                let p: StartAgentLoginParams = parse_params(params)?;
                let start = self
                    .agent_accounts
                    .start_login(p.harness)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&start)
            }
            methods::COMPLETE_AGENT_LOGIN => {
                let p: CompleteAgentLoginParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .complete_login(&p.login_id, &p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::POLL_AGENT_LOGIN => {
                let p: LoginIdParams = parse_params(params)?;
                let poll = self
                    .agent_accounts
                    .poll_login(&p.login_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&poll)
            }
            methods::CANCEL_AGENT_LOGIN => {
                let p: LoginIdParams = parse_params(params)?;
                self.agent_accounts.cancel_login(&p.login_id);
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::GET_PI_SETTINGS => {
                let p: PiSettingsParams = parse_params(params)?;
                let snapshot = self
                    .pi_management
                    .snapshot(p.scope, p.project_path.as_deref())
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&snapshot)
            }
            methods::SET_PI_SETTING => {
                let p: SetPiSettingParams = parse_params(params)?;
                let snapshot = self
                    .pi_management
                    .set_setting(p.scope, p.project_path.as_deref(), &p.key, p.value)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&snapshot)
            }
            methods::SET_PI_CREDENTIAL => {
                let p: SetPiCredentialParams = parse_params(params)?;
                let snapshot = self
                    .pi_management
                    .set_api_key(&p.provider, &p.key, p.project_path.as_deref(), p.scope)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&snapshot)
            }
            methods::REMOVE_PI_CREDENTIAL => {
                let p: RemovePiCredentialParams = parse_params(params)?;
                let snapshot = self
                    .pi_management
                    .remove_credential(&p.provider, p.project_path.as_deref(), p.scope)
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&snapshot)
            }
            methods::SET_PI_OPENAI_COMPATIBLE => {
                let p: SetPiOpenAiCompatibleParams = parse_params(params)?;
                let snapshot = self
                    .pi_management
                    .set_openai_compatible(
                        &p.base_url,
                        p.api_key.as_deref(),
                        p.project_path.as_deref(),
                        p.scope,
                    )
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&snapshot)
            }
            methods::PI_PACKAGE_ACTION => {
                let p: PiPackageActionParams = parse_params(params)?;
                let snapshot = self
                    .pi_management
                    .package_action(&p.action, &p.source, p.scope, p.project_path.as_deref())
                    .await
                    .map_err(RpcError::Failed)?;
                RpcReply::value(&snapshot)
            }
            methods::UPLOAD_CHUNK => {
                let p: UploadChunkParams = parse_params(params)?;
                self.uploads
                    .append(&p.upload_id, &p.data, p.seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_COMMIT => {
                let p: UploadCommitParams = parse_params(params)?;
                let path = self
                    .uploads
                    .commit(&p.upload_id, &p.file_name)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "path": path }))
            }
            methods::READ_ATTACHMENT_CHUNK => {
                let p: ReadAttachmentChunkParams = parse_params(params)?;
                // Path jail: the uploads dir plus every workspace-known chat cwd.
                let roots: Vec<std::path::PathBuf> = self
                    .workspace
                    .doc()
                    .read_chats()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|chat| chat.cwd)
                    .map(std::path::PathBuf::from)
                    .collect();
                let chunk = self
                    .uploads
                    .read_chunk(&p.path, p.offset, &roots)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&chunk)
            }
            // Nova Engine peer-network settings (IPC-only; never peer-forwarded).
            comet_nova::methods::NOVA_LOCAL_DEVICE => {
                let nova = self.nova()?;
                RpcReply::value(&serde_json::json!({
                    "deviceId": nova.device_id(),
                    "name": nova.device_name(),
                    "port": nova.listener_port(),
                    "ticket": nova.ticket()?,
                }))
            }
            comet_nova::methods::NOVA_BEGIN_PAIRING => {
                Ok(RpcReply::Value(self.nova()?.begin_pairing()))
            }
            comet_nova::methods::NOVA_CANCEL_PAIRING => {
                Ok(RpcReply::Value(self.nova()?.cancel_pairing()))
            }
            comet_nova::methods::NOVA_LIST_PEERS => RpcReply::value(&self.nova()?.list_peers()),
            comet_nova::methods::NOVA_WATCH_PEERS => Ok(RpcReply::Stream(nova_peer_watch_stream(
                self.nova()?.clone(),
            ))),
            comet_nova::methods::NOVA_PAIR_PEER => {
                let p: PairPeerParams = parse_params(params)?;
                self.nova()?
                    .pair_peer(&p.endpoint, &p.code)
                    .await
                    .map(RpcReply::Value)
            }
            comet_nova::methods::NOVA_UPDATE_PEER => {
                let p: UpdatePeerParams = parse_params(params)?;
                let nova = self.nova()?.clone();
                let result = nova
                    .update_peer(&p.device_id, p.name, &p.endpoint, p.role)
                    .map(RpcReply::Value);
                if result.is_ok() {
                    nova.invalidate(&p.device_id).await;
                }
                result
            }
            comet_nova::methods::NOVA_TEST_PEER => {
                let p: IdParams = parse_params(params)?;
                let client = self.nova()?.dial(&p.device_id).await?;
                client
                    .call(methods::LIST_HARNESSES, serde_json::Value::Null)
                    .await
                    .map(|_| RpcReply::Value(serde_json::json!({"ok": true})))
            }
            comet_nova::methods::NOVA_REVOKE_PEER => {
                let p: IdParams = parse_params(params)?;
                let nova = self.nova()?.clone();
                let result = nova.revoke_peer(&p.device_id).map(RpcReply::Value);
                nova.invalidate(&p.device_id).await;
                result
            }
            comet_nova::methods::NOVA_FORGET_PEER => {
                let p: IdParams = parse_params(params)?;
                let nova = self.nova()?.clone();
                let result = nova.forget_peer(&p.device_id).map(RpcReply::Value);
                nova.invalidate(&p.device_id).await;
                result
            }
            comet_nova::methods::NOVA_SCAN => {
                let p: ScanParams = parse_params(params)?;
                Ok(RpcReply::Stream(self.nova()?.scan(
                    p.ranges,
                    p.port,
                    p.allow_public,
                )?))
            }
            comet_nova::methods::NOVA_SYNC_HEADS => {
                let sync = PeerSync::new(
                    self.nova()?.clone(),
                    self.workspace.clone(),
                    self.doc_host.clone(),
                );
                RpcReply::value(
                    &sync
                        .heads()
                        .map_err(|error| RpcError::Failed(error.to_string()))?,
                )
            }
            comet_nova::methods::NOVA_SYNC_APPLY => {
                let exchange: SyncExchange = parse_params(params)?;
                let sync = PeerSync::new(
                    self.nova()?.clone(),
                    self.workspace.clone(),
                    self.doc_host.clone(),
                );
                RpcReply::value(
                    &sync
                        .apply(exchange)
                        .map_err(|error| RpcError::Failed(error.to_string()))?,
                )
            }
            other => Err(RpcError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI's Switch/Forget calls send `{id, accountId, harness}` (+ optional
    /// `targetDeviceId`); the extra fields must be tolerated, `accountId` wins.
    #[test]
    fn agent_account_params_accept_ui_shape() {
        let p: AgentAccountParams = parse_params(serde_json::json!({
            "id": "acct-1",
            "accountId": "acct-1",
            "harness": "claude-code",
            "targetDeviceId": "dev-2",
        }))
        .expect("ui param shape");
        assert_eq!(p.account_id, "acct-1");
        assert_eq!(p.harness, HarnessId::ClaudeCode);
    }

    #[test]
    fn local_device_is_not_forwardable() {
        assert!(!forwardable(methods::LOCAL_DEVICE));
        assert!(forwardable(methods::QUEUE_COMMAND));
        assert!(forwardable(methods::TOOL_DIFF));
    }
}
