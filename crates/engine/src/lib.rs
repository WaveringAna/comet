//! nova-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads,
//! agent accounts, and direct Nova peer transport land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use nova_proto::HarnessId;

use nova_sync::DocsStore;

pub mod agent_accounts;
pub mod diff_sync;
pub mod doc_host;
pub mod ephemeral_diffs;
pub mod instance_lock;
pub mod nova;
pub mod peer_sync;
pub mod pi_management;
pub mod projects;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use diff_sync::{CheckoutDiffSync, DiffSnapshot, capture_diff};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig};
pub use ephemeral_diffs::EphemeralDiffStore;
pub use instance_lock::InstanceLock;
pub use pi_management::PiManagement;
pub use projects::ProjectsSync;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_host::{
    DEFAULT_ORG_ID, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] nova_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] nova_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] nova_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.nova-native`, dev `~/.nova-native-dev`).
    pub data_dir: PathBuf,
    /// Optional Nova release server. No network update check runs when absent.
    pub update_url: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Direct Nova listener port.
    pub nova_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub diff_sync: CheckoutDiffSync,
    /// Private, process-lifetime tool previews. Never journaled or synced.
    pub ephemeral_diffs: Arc<EphemeralDiffStore>,
    pub projects_sync: ProjectsSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub device_id: String,
    pub nova: nova::NovaHost,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<nova_update::Updater>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
    ) -> Result<Self, EngineError> {
        Self::assemble_on_port(data_dir, registry, default_harness, nova_listener_port())
    }

    /// Assemble with an explicit Nova port. Used by the runtime configuration and
    /// isolated multi-engine integration tests; `0` requests an ephemeral UDP port.
    pub fn assemble_on_port(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        nova_port: u16,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        let nova = nova::NovaHost::load(data_dir, std::env::consts::OS, &device_id, nova_port)?;
        // Keep the former default auth namespace as the local profile directory
        // so removing hosted identity does not make existing chats disappear.
        let org_dir = data_dir
            .join("orgs")
            .join(DEFAULT_ORG_ID)
            .join(DEFAULT_USER_ID);
        let store = Arc::new(DocsStore::open(&org_dir)?);
        let journal = Arc::new(RunJournal::open(org_dir.join("journals"))?);
        let ephemeral_diffs = Arc::new(EphemeralDiffStore::open(data_dir)?);
        let sessions = SessionsEngine::new(
            device_id.clone(),
            journal,
            registry.clone(),
            ephemeral_diffs.clone(),
        );
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(),
                platform: std::env::consts::OS.to_string(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        let repos = Repos::new(data_dir, &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::new(data_dir);
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id);
        let projects_sync = ProjectsSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            ephemeral_diffs,
            projects_sync,
            uploads,
            agent_accounts,
            device_id,
            nova,
            updater: std::sync::Mutex::new(None),
            _instance_lock: lock,
        })
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater(&self, updater: nova_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<nova_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.terminals.clone(),
            self.diff_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
        )
        .with_nova(self.nova.clone());
        if let Some(updater) = self.updater() {
            rpc = rpc.with_updater(updater);
        }
        Arc::new(rpc)
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    pub async fn shutdown(&self) {
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.ephemeral_diffs.clear();
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

/// A fully assembled identity-scoped engine with its direct Nova listener.
pub struct EngineRuntime {
    core: EngineCore,
    nova_listener: tokio::task::JoinHandle<()>,
    discovery_listener: tokio::task::JoinHandle<()>,
    peer_sync: tokio::task::JoinHandle<()>,
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub async fn shutdown(&self) {
        self.peer_sync.abort();
        self.discovery_listener.abort();
        self.nova_listener.abort();
        self.core.shutdown().await;
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Open the local engine and its direct peer synchronization transport.
    pub async fn assemble_runtime(config: &EngineConfig) -> anyhow::Result<EngineRuntime> {
        let core = EngineCore::assemble_on_port(
            &config.data_dir,
            Arc::new(default_registry()),
            config.default_harness,
            config.nova_port,
        )?;
        // Optional release checker: polls the configured server on a 6h cadence; headless
        // installs with NOVA_AUTO_UPDATE=1 apply + restart themselves — gated
        // on quiescence so a restart never lands under a live run or open PTY.
        let quiescent: nova_update::QuiescentCheck = {
            let sessions = core.sessions.clone();
            let terminals = core.terminals.clone();
            Arc::new(move || !sessions.any_active() && !terminals.any_open())
        };
        if let Some(update_url) = config.update_url.clone() {
            core.set_updater(nova_update::Updater::spawn(update_url, Some(quiescent)));
        }
        tracing::info!(device_id = %core.device_id, "engine core assembled");

        // Iroh binds UDP on the Nova port. The signed discovery-only WebSocket
        // probe may use the same numeric TCP port without carrying RPC traffic.
        let endpoint = core.nova.bind_endpoint(true).await?;
        let discovery_socket = tokio::net::TcpListener::bind((
            std::net::Ipv4Addr::UNSPECIFIED,
            core.nova.listener_port(),
        ))
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "nova listener bind on port {} failed: {error}",
                core.nova.listener_port()
            )
        })?;
        let nova_service = core.rpc_service();
        let nova_trust = core.nova.trust();
        let nova_identity = core.nova.identity();
        let nova_pairing = core.nova.pairing();
        let nova_port = core.nova.listener_port();
        let discovery_listener = tokio::spawn(nova_network::transport::serve_discovery_listener(
            discovery_socket,
            core.nova.identity(),
            endpoint.clone(),
        ));
        let nova_listener = tokio::spawn(nova_network::transport::serve_iroh_endpoint(
            endpoint,
            nova_service,
            nova_trust,
            nova_identity,
            nova_pairing,
        ));
        tracing::info!(
            port = nova_port,
            "nova iroh endpoint and discovery probe ready"
        );

        let peer_sync = peer_sync::PeerSync::new(
            core.nova.clone(),
            core.workspace.clone(),
            core.doc_host.clone(),
        )
        .spawn();

        Ok(EngineRuntime {
            core,
            nova_listener,
            discovery_listener,
            peer_sync,
        })
    }

    /// Run until ctrl-c: sessions engine, direct Nova listener, and IPC server.
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        let runtime = Self::assemble_runtime(&config).await?;

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let server = serve_ipc(config.ipc_port, runtime.core().rpc_service()).await?;

        shutdown_signal().await?;
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

fn nova_listener_port() -> u16 {
    std::env::var("NOVA_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(27655)
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
pub async fn serve_ipc(
    port: u16,
    service: std::sync::Arc<dyn nova_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(nova_rpc::serve_ws_listener(listener, service)))
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name() -> String {
    std::env::var("NOVA_DEVICE_NAME")
        .ok()
        .or_else(|| std::env::var("COMET_DEVICE_NAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(command_hostname)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        Ok(_) | Err(_) => {
            let id = new_id();
            std::fs::write(&path, &id)?;
            Ok(id)
        }
    }
}
