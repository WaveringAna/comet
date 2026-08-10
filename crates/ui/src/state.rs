//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The UI talks the same typed RPC whether the engine is in-process or a separate
//! daemon (ARCHITECTURE §1). [`EngineHandle::bootstrap`] probes the localhost IPC
//! port, mirroring comet: if an engine is listening it connects over WebSocket
//! ([`RemoteEngine`]); otherwise it embeds one via [`EngineCore::assemble`] and an
//! in-memory RPC transport ([`InProcessEngine`]) — same envelopes, same dispatch.
//!
//! ## Async bridging
//! `bootstrap` runs on tokio via `gpui_tokio::Tokio::spawn`. Once an [`RpcClient`]
//! exists, its `call`/`subscribe` futures are runtime-agnostic (tokio channels),
//! so subscription pumps run on gpui's own executor via `cx.spawn` and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use serde::de::DeserializeOwned;

use comet_doc::{MessagePart, MessageRole, SessionMessageEntry};
use comet_engine::{Engine, EngineConfig, EngineRuntime};
use comet_proto::{
    Chat, ChatIndicator, CollaborationSession, Device, HarnessId, Project, Session, SessionStatus,
};
use comet_rpc::{RpcClient, connect_ws, memory_client, methods};

// ---------------------------------------------------------------------------
// Hot reload (dev-supervisor handoff). The Settings → Developer toggle or
// `NOVA_HOTRELOAD=1` opts the headed app into a three-part contract used by
// scripts/nova-dev.sh: persist the selected chat on every change, restore it
// once the chats watch lands after attach, and print a ready marker on stdout
// at the first engine attach so the supervisor knows the old window can be
// retired. Normal runs never touch any of this.
fn hotreload_enabled() -> bool {
    std::env::var_os("NOVA_HOTRELOAD").is_some()
}

/// The ready marker is process-global and printed exactly once: the supervisor
/// only reads the first attach, and reconnects must not re-signal.
static HOTRELOAD_ANNOUNCED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// ---------------------------------------------------------------------------
/// Chars per token for the live readout. Prose and code both land near 4, so
/// a single constant is within noise of the exact end-of-turn figure — good
/// enough for a live meter that the real usage count replaces on completion.
const CHARS_PER_TOKEN: f32 = 4.0;

/// Live tok/s from streamed output. `None` on nothing-streamed-yet or a
/// sub-frame elapsed window (the latter guards a div-by-near-zero flash on
/// the very first frame of a turn).
pub fn live_tokens_per_sec(streamed_chars: u64, elapsed_secs: f32) -> Option<f32> {
    if streamed_chars == 0 || elapsed_secs <= 0.0 {
        return None;
    }
    Some(streamed_chars as f32 / CHARS_PER_TOKEN / elapsed_secs)
}

// Engine handle
// ---------------------------------------------------------------------------

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.comet-native`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to probe / serve.
    pub ipc_port: u16,
    /// Direct Nova listener port used when this window embeds the engine.
    pub nova_port: u16,
    /// Optional Nova release server.
    pub update_url: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// One of the two ways to own an engine connection. Both end at an [`RpcClient`]
/// speaking the identical protocol — the trait only differs in provenance and
/// teardown.
#[async_trait]
trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    /// Graceful teardown (drains runs / flushes docs for the in-process engine).
    async fn shutdown(&self);
}

/// Embedded engine: owns the [`EngineCore`] and an in-memory RPC loop.
struct InProcessEngine {
    runtime: Arc<tokio::sync::Mutex<Option<EngineRuntime>>>,
    /// Serves this engine to other viewports over the IPC port. `None` when the
    /// port was already taken — the window still works over its own transport.
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: RpcClient,
}

#[async_trait]
impl EngineBackend for InProcessEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }
    async fn shutdown(&self) {
        // Stop accepting first: a viewport must not connect midway through the
        // drain and queue work against stores that are closing.
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

/// External daemon over `ws://127.0.0.1:{port}`.
struct RemoteEngine {
    client: RpcClient,
    url: String,
}

#[async_trait]
impl EngineBackend for RemoteEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }
    fn mode(&self) -> EngineMode {
        EngineMode::Remote {
            url: self.url.clone(),
        }
    }
    async fn shutdown(&self) {
        // The daemon outlives this viewport; nothing to tear down.
    }
}

/// Cheaply clonable handle to whichever backend won the probe.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
}

impl EngineHandle {
    /// Probe the IPC port and connect (daemon listening) or embed (nothing there).
    /// Must run on the tokio runtime (`Tokio::spawn`): both transports spawn
    /// tokio tasks.
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
        let url = format!("ws://127.0.0.1:{}", config.ipc_port);
        let probe = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            tokio::net::TcpStream::connect(("127.0.0.1", config.ipc_port)),
        )
        .await;
        if matches!(probe, Ok(Ok(_))) {
            tracing::info!(%url, "engine daemon detected; connecting");
            match connect_ws(&url).await {
                Ok(client) => {
                    return Ok(EngineHandle {
                        inner: Arc::new(RemoteEngine { client, url }),
                    });
                }
                // Something is on the port but it is not an engine (or it is
                // wedged). Fall through and embed: a stranger holding 27654
                // should cost other viewports, not this window.
                Err(err) => tracing::warn!(%url, error = %err, "not an engine; embedding instead"),
            }
        }

        tracing::info!(data_dir = %config.data_dir.display(), "no daemon on port; embedding engine");
        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            update_url: config.update_url,
            ipc_port: config.ipc_port,
            nova_port: config.nova_port,
            default_harness: config.default_harness,
        };
        let engine_runtime = Engine::assemble_runtime(&engine_config).await?;
        let service = engine_runtime.core().rpc_service();
        let client = memory_client(service.clone());

        // Serve the same service on the IPC port so a terminal viewport can
        // attach to this window's engine with no setup.
        // Best-effort — losing the bind race with another engine costs other
        // viewports, not this one.
        let ipc_task = match comet_engine::serve_ipc(engine_config.ipc_port, service).await {
            Ok(task) => Some(task),
            Err(err) => {
                tracing::warn!(
                    port = engine_config.ipc_port,
                    error = %err,
                    "IPC port unavailable; other viewports cannot attach to this window"
                );
                None
            }
        };
        let runtime = Arc::new(tokio::sync::Mutex::new(Some(engine_runtime)));
        Ok(EngineHandle {
            inner: Arc::new(InProcessEngine {
                runtime,
                ipc_task,
                client,
            }),
        })
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `comet_proto::view` so the
// terminal viewport (`comet-tui`) shares one implementation and one test suite
// with this one — a sort order that differs per surface is a bug. Re-exported
// here because every call site in this crate reads them as `state::…`.
pub use comet_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    project_label, sort_active, sort_chats, sort_projects, sort_tabs,
};

// ---------------------------------------------------------------------------
// AppState entity
// ---------------------------------------------------------------------------

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    pub devices: Vec<Device>,
    workspace_devices: Vec<Device>,
    nova_devices: Vec<Device>,
    /// Sorted (see [`sort_projects`]).
    pub projects: Vec<Project>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    /// Host-local Pi parent/child collaboration snapshots.
    pub collaborations: Vec<CollaborationSession>,
    /// The project whose tabs fill the main area. Healed by [`Self::apply_projects`]
    /// when the row vanishes; selecting a chat implies its project.
    pub selected_project: Option<String>,
    pub selected_chat: Option<String>,
    /// Boot auto-select happened (or a manual selection superseded it).
    pub auto_selected: bool,
    /// Joined transcript of the selected chat (continuations folded engine-side).
    pub transcript: Vec<SessionMessageEntry>,
    /// When the selected chat's turn entered Working — the clock behind the
    /// LIVE tok/s readout. Deliberately NOT a CRDT field: it is ephemeral,
    /// viewport-local, and derives from streams the view already gets, so it
    /// never threatens the LWW-once-per-turn `ChatUsage` on the chat row. Set
    /// from `apply_sessions`/`apply_transcript`; `None` while idle.
    working_since: Option<std::time::Instant>,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully).
    pub local_device_id: Option<String>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<comet_update::UpdateStatus>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    /// Settings → Developer hot-reload toggle (persisted in ui-settings.json,
    /// pushed in by the shell). The `NOVA_HOTRELOAD` env var forces the same
    /// behavior on — see [`hotreload_enabled`].
    hotreload: bool,
    engine: Option<EngineHandle>,
    watch_tasks: Vec<Task<()>>,
    transcript_task: Option<Task<()>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            devices: Vec::new(),
            workspace_devices: Vec::new(),
            nova_devices: Vec::new(),
            projects: Vec::new(),
            chats: Vec::new(),
            working_since: None,
            sessions: Vec::new(),
            collaborations: Vec::new(),
            selected_project: None,
            selected_chat: None,
            transcript: Vec::new(),
            echoes: HashMap::new(),
            local_device_id: None,
            update: None,
            data_dir: None,
            hotreload: false,
            engine: None,
            watch_tasks: Vec::new(),
            transcript_task: None,
            auto_selected: false,
        }
    }

    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            // Selected chat vanished (deleted elsewhere): drop selection + transcript.
            self.selected_chat = None;
            self.transcript.clear();
            self.transcript_task = None;
        }
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
        self.refresh_working_clock();
    }

    pub fn apply_collaborations(&mut self, collaborations: Vec<CollaborationSession>) {
        self.collaborations = collaborations;
    }

    pub fn apply_projects(&mut self, mut projects: Vec<Project>) {
        sort_projects(&mut projects);
        self.projects = projects;
        // Heal a vanished selection (project deleted elsewhere): fall back to the
        // first project; its chats died with it, so a matching chat selection is
        // healed by the accompanying chats frame (`apply_chats`).
        if let Some(selected) = &self.selected_project
            && !self.projects.iter().any(|s| &s.id == selected)
        {
            self.selected_project = self.projects.first().map(|s| s.id.clone());
        }
        // First frame with no selection yet: pick the first project so the shell
        // never renders an empty main area while projects exist.
        if self.selected_project.is_none() {
            self.selected_project = self.projects.first().map(|s| s.id.clone());
        }
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: comet_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_devices(&mut self, devices: Vec<Device>) {
        self.workspace_devices = devices;
        self.rebuild_devices();
    }

    pub fn apply_nova_devices(&mut self, devices: Vec<Device>) {
        self.nova_devices = devices;
        self.rebuild_devices();
    }

    fn rebuild_devices(&mut self) {
        let mut merged = self.workspace_devices.clone();
        for peer in &self.nova_devices {
            if let Some(existing) = merged.iter_mut().find(|device| device.id == peer.id) {
                existing.name = peer.name.clone();
                existing.platform = peer.platform.clone();
                existing.last_seen_at = peer.last_seen_at.or(existing.last_seen_at);
            } else {
                merged.push(peer.clone());
            }
        }
        merged.sort_by(|a, b| a.id.cmp(&b.id));
        self.devices = merged;
    }

    pub fn apply_update(&mut self, status: comet_update::UpdateStatus) {
        self.update = Some(status);
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.refresh_working_clock();
    }

    /// Add an optimistic user echo (composer send path).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Flip the live-rate clock with the Working state. Gated on the SESSION
    /// status, not on a streaming message: through a tool loop the session
    /// stays Working across the fleeting gaps between assistant rounds, so the
    /// clock spans the whole turn instead of resetting on every burst — which
    /// would re-create the final-burst spike this readout exists to kill.
    fn refresh_working_clock(&mut self) {
        let working = self.selected_chat.as_deref().is_some_and(|id| {
            self.sessions
                .iter()
                .any(|s| s.chat_id == *id && s.status == SessionStatus::Working)
        });
        match (self.working_since.is_some(), working) {
            (false, true) => self.working_since = Some(std::time::Instant::now()),
            (true, false) => self.working_since = None,
            _ => {}
        }
    }

    /// Is the selected chat's turn running right now? Gates the live ticker's
    /// re-render so the rate's denominator keeps moving through silent
    /// stretches (a long think between tool rounds) without a hot loop.
    pub fn working(&self) -> bool {
        self.working_since.is_some()
    }

    /// Chars of assistant output in the current turn — everything after the
    /// last User message. A fresh prompt appends a User message and rolls the
    /// boundary forward, so completed tool-round messages of THIS turn stay
    /// counted while earlier turns drop out.
    ///
    /// Counts text AND the tool-call JSON, because pi's real `output` usage
    /// (which the settled whole-turn average comes from) includes the tokens a
    /// tool round spends on arguments — a text-only counter would undercount a
    /// tool-heavy turn. Thinking is deliberately NOT here: the engine drops
    /// `ReasoningDelta` from the fold (it is not rendered, matching comet), so
    /// the synced transcript never carries it. The live readout is a
    /// best-effort viewport estimate; it is honest that reasoning is invisible
    /// to it, which is why the exact per-message-end figure still wins on
    /// completion.
    pub fn current_turn_streamed(&self) -> u64 {
        let start = self
            .transcript
            .iter()
            .rposition(|e| e.role == MessageRole::User)
            .map_or(0, |i| i + 1);
        self.transcript[start..]
            .iter()
            .filter(|e| e.role == MessageRole::Assistant)
            .flat_map(|e| e.parts.iter())
            .map(|p| match p {
                MessagePart::Text { text, .. } => text.chars().count() as u64,
                MessagePart::Tool { call, .. } => {
                    serde_json::to_string(call).map_or(0, |s| s.chars().count() as u64)
                }
                _ => 0,
            })
            .sum()
    }

    /// LIVE generation speed while the selected chat's turn streams. Chars→
    /// tokens is an estimate (pi reports real usage only at message_end), so
    /// this is the "watch it move" number; the settled whole-turn average on
    /// the chat row replaces it once the turn completes.
    pub fn live_tokens_per_sec(&self, now: std::time::Instant) -> Option<f32> {
        let started = self.working_since?;
        let secs = now.saturating_duration_since(started).as_secs_f32();
        live_tokens_per_sec(self.current_turn_streamed(), secs)
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_project_row(&self) -> Option<&Project> {
        let id = self.selected_project.as_deref()?;
        self.projects.iter().find(|s| s.id == id)
    }

    pub fn project_row(&self, project_id: &str) -> Option<&Project> {
        self.projects.iter().find(|s| s.id == project_id)
    }

    pub fn project_for_chat(&self, chat: &Chat) -> Option<&Project> {
        self.project_row(chat.project_id.as_deref()?)
    }

    /// Non-archived chats of a project in tab (creation) order. Chats with a
    /// dangling/missing `project_id` are invisible by construction.
    pub fn chats_in_project(&self, project_id: &str) -> Vec<&Chat> {
        let mut chats: Vec<&Chat> = self
            .visible_chats()
            .filter(|c| c.project_id.as_deref() == Some(project_id))
            .collect();
        sort_tabs(&mut chats);
        chats
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    /// Host-presence check: was this device observed by recent direct sync?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The local device is trivially online; unknown devices
    /// get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// Does the selected project's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_project_git(&self) -> bool {
        self.selected_project_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (tab dots, Active list).
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Sessions list: every non-archived chat of a LIVE project,
    /// on any device — idle included — in pure recency order (status drives
    /// the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| {
                c.project_id
                    .as_deref()
                    .is_some_and(|id| self.project_row(id).is_some())
            })
            .map(|c| (display_status(c, self.session_for(&c.id), now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row.
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn selected_collaboration(&self) -> Option<&CollaborationSession> {
        let chat_id = self.selected_chat.as_deref()?;
        self.collaborations
            .iter()
            .find(|collaboration| collaboration.chat_id == chat_id)
    }

    pub fn gate(&self) -> GatePhase {
        gate_phase(&self.connection)
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    // ---- gpui glue ----

    /// Kick off (or retry) the engine bootstrap: probe → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(state: Entity<AppState>, config: EngineBootConfig, cx: &mut App) {
        let data_dir = config.data_dir.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.data_dir = Some(data_dir);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, EngineHandle::bootstrap(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine: mark Ready and start the standing watches.
    /// Methods the engine doesn't serve yet fail their subscribe and are skipped
    /// gracefully.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        self.connection = ConnectionStatus::Ready;
        self.engine = Some(handle.clone());
        self.watch_tasks = vec![
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_SESSIONS,
                AppState::apply_sessions,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_COLLABORATIONS,
                AppState::apply_collaborations,
            ),
            spawn_chats_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_DEVICES,
                AppState::apply_devices,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::WATCH_PROJECTS,
                AppState::apply_projects,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                methods::UPDATE_STATUS,
                AppState::apply_update,
            ),
            spawn_local_device_probe(cx, handle.clone()),
            spawn_nova_devices_watch(cx, handle.clone()),
        ];
        // Re-subscribe the transcript if a chat was already selected (reconnect path).
        if let Some(chat_id) = self.selected_chat.clone() {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        if self.hotreload_active() {
            // Tell the dev supervisor a rebuilt window is attached and live;
            // it may now retire the previous process.
            if !HOTRELOAD_ANNOUNCED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                use std::io::Write as _;
                println!("nova-hotreload-ready");
                let _ = std::io::stdout().flush();
            }
            // The chats watch lands asynchronously after attach, so restore
            // the previous selection once rows exist (bounded wait — a fresh
            // data dir with no chats must not spin forever).
            cx.spawn(async move |this, cx| {
                for _ in 0..50 {
                    let landed = this
                        .update(cx, |s, _| !s.chats.is_empty() || s.selected_chat.is_some())
                        .unwrap_or(true);
                    if landed {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;
                }
                this.update(cx, |s, cx| s.restore_hotreload(cx)).ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// Hot-reload state file (`{data_dir}/hotreload-state.json`) — only Some
    /// when the supervisor contract is active.
    fn hotreload_path(&self) -> Option<PathBuf> {
        if !self.hotreload_active() {
            return None;
        }
        self.data_dir
            .as_ref()
            .map(|dir| dir.join("hotreload-state.json"))
    }

    /// Persist the current selection so a replacement process can reopen the
    /// same chat. Called from `select_chat`; best-effort, never fatal.
    fn persist_hotreload(&self) {
        let Some(path) = self.hotreload_path() else {
            return;
        };
        let body = serde_json::json!({ "selected_chat": self.selected_chat });
        let _ = std::fs::write(path, body.to_string());
    }

    /// Reopen the chat the retired process had open, if it still exists.
    fn restore_hotreload(&mut self, cx: &mut Context<Self>) {
        if self.selected_chat.is_some() {
            return;
        }
        let Some(path) = self.hotreload_path() else {
            return;
        };
        let Ok(body) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
            return;
        };
        let Some(id) = value
            .get("selected_chat")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return;
        };
        if self.chats.iter().any(|c| c.id == id) {
            self.select_chat(Some(id), cx);
        }
    }

    /// Hot-reload participation: the Developer settings toggle or the
    /// supervisor's `NOVA_HOTRELOAD` env var.
    fn hotreload_active(&self) -> bool {
        self.hotreload || hotreload_enabled()
    }

    /// Settings → Developer pushes the persisted flag in at boot and on change.
    pub fn set_hotreload(&mut self, on: bool) {
        self.hotreload = on;
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its project and marks it
    /// seen (a global-list click must switch the tab strip too).
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.auto_selected = true;
        self.transcript.clear();
        self.transcript_task = None;
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its project; `select_chat(None)` (the new-session
            // canvas) stays within the current project.
            if let Some(project_id) = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.project_id.clone())
            {
                self.selected_project = Some(project_id);
            }
            self.mark_chat_seen(id, cx);
        }
        if let (Some(chat_id), Some(handle)) = (chat_id, self.engine.clone()) {
            self.transcript_task = Some(spawn_transcript_watch(cx, handle, chat_id));
        }
        self.persist_hotreload();
        cx.notify();
    }

    /// Select a project; the caller (shell) decides which chat to land on.
    pub fn select_project(&mut self, project_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_project == project_id {
            return;
        }
        self.selected_project = project_id;
        cx.notify();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let params = serde_json::json!({ "op": "markChatSeen", "chatId": chat_id });
            if let Err(err) = handle.client().call(methods::MUTATE, params).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }
}

/// Subscribe to a watch method and pump each frame through `apply`. Runs on the
/// gpui executor; ends when the stream closes or the entity is released.
/// Chats watch with boot auto-select: comet's `/` route redirected to the
/// last-used chat; we approximate by selecting the most recent unarchived chat
/// on the first frame when nothing is selected yet (manual selection wins).
fn spawn_chats_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(methods::WATCH_CHATS, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(error = %err, "chats watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: Vec<Chat> = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed chats frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                state.apply_chats(parsed);
                if state.selected_chat.is_none() && !state.auto_selected {
                    let most_recent = state
                        .chats
                        .iter()
                        .find(|c| !c.archived)
                        .map(|c| c.id.clone());
                    if let Some(chat_id) = most_recent {
                        state.auto_selected = true;
                        state.select_chat(Some(chat_id), cx);
                    }
                }
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

fn spawn_watch<T: DeserializeOwned + 'static>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    method: &'static str,
    apply: fn(&mut AppState, T),
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match handle
            .client()
            .subscribe(method, serde_json::json!({}))
            .await
        {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(method, error = %err, "watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: T = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(method, error = %err, "dropping malformed watch frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                apply(state, parsed);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

/// Best-effort `LocalDevice` probe: fills `local_device_id` for the "This
/// device" badge. Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(value) = handle
            .client()
            .call("LocalDevice", serde_json::json!({}))
            .await
        else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        let id = value
            .get("id")
            .or_else(|| value.get("deviceId"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if let Some(id) = id {
            this.update(cx, |state, cx| {
                state.local_device_id = Some(id);
                cx.notify();
            })
            .ok();
        }
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NovaPeerDevice {
    device_id: String,
    name: String,
    platform: String,
    revoked: bool,
    paired_at: chrono::DateTime<Utc>,
}

/// Merge paired Nova identities into the ordinary device selectors. Workspace rows still
/// carry projects/chats; this probe makes newly paired engines selectable immediately,
/// without waiting for the removed hosted workspace registry.
fn spawn_nova_devices_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(mut values) = handle
            .client()
            .subscribe("NovaWatchPeers", serde_json::Value::Null)
            .await
        else {
            return;
        };
        while let Some(value) = values.recv().await {
            let Ok(peers) = serde_json::from_value::<Vec<NovaPeerDevice>>(value) else {
                continue;
            };
            let devices = peers
                .into_iter()
                .filter(|peer| !peer.revoked)
                .map(|peer| Device {
                    id: peer.device_id,
                    name: peer.name,
                    platform: peer.platform,
                    last_seen_at: None,
                    created_at: Some(peer.paired_at),
                    version: None,
                })
                .collect();
            if this
                .update(cx, |state, cx| {
                    state.apply_nova_devices(devices);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let params = serde_json::json!({ "chatId": chat_id });
        let mut rx = match handle
            .client()
            .subscribe(methods::WATCH_DOC_MESSAGES, params)
            .await
        {
            Ok(rx) => rx,
            Err(err) => {
                tracing::warn!(%chat_id, error = %err, "transcript watch failed");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let entries: Vec<SessionMessageEntry> = match serde_json::from_value(value) {
                Ok(entries) => entries,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed transcript frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                // Guard against a stale pump racing a newer selection.
                if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    state.apply_transcript(entries);
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use comet_engine::{EngineCore, default_registry};
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `comet_proto::view`.
    use comet_proto::SessionStatus;

    /// A localhost port that was just free (bind :0, read, drop).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            nova_port: free_port().await,
            update_url: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            nova_port: free_port().await,
            update_url: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way `comet-tui` does, and speak the same protocol.
        let attached = connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = attached
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // The port probe only proves *something* is listening. A process that
        // accepts TCP and never speaks WebSocket used to hang the dial forever;
        // now it times out and we embed instead, losing only the ability to
        // serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            nova_port: free_port().await,
            update_url: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            handle
                .client()
                .call(methods::LIST_HARNESSES, serde_json::json!({}))
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `comet headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(comet_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            nova_port: free_port().await,
            update_url: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        let harnesses = handle
            .client()
            .call(methods::LIST_HARNESSES, serde_json::json!({}))
            .await
            .unwrap();
        assert!(harnesses.as_array().is_some_and(|h| !h.is_empty()));
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_command: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            project_id: None,
            last_seen_at: None,
            usage: None,
        }
    }

    fn project(id: &str, device_id: &str, path: &str, created_min: i64) -> Project {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Project {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
        }
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_by_recency_only_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let c = chat("c", 0, Some(5)); // AwaitingInput
        let d = chat("d", 0, Some(1)); // Working
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a", "c", "d"], "recency desc, status ignored");

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn tabs_order_by_creation_not_activity() {
        let a = chat("a", 5, Some(100)); // created later, very active
        let b = chat("b", 1, Some(2));
        let mut tabs = vec![&a, &b];
        sort_tabs(&mut tabs);
        let order: Vec<&str> = tabs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["b", "a"]);
    }

    #[test]
    fn apply_projects_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_projects(vec![
            project("s2", "dev", "/b", 2),
            project("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.projects.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first project.
        assert_eq!(state.selected_project.as_deref(), Some("s1"));
        state.selected_project = Some("s2".into());
        // Vanished selection heals to the first project.
        state.apply_projects(vec![project("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_project.as_deref(), Some("s1"));
        // No projects at all: selection clears.
        state.apply_projects(vec![]);
        assert_eq!(state.selected_project, None);
    }

    #[test]
    fn live_tokens_per_sec_derivation() {
        // 800 chars at ~4 chars/token over 10s → 20 tok/s.
        assert_eq!(
            live_tokens_per_sec(800, 10.0),
            Some(20.0),
            "chars ÷ chars-per-token ÷ seconds"
        );
        // Nothing streamed yet: no rate.
        assert_eq!(live_tokens_per_sec(0, 5.0), None);
        // Sub-frame elapsed window (the first instant of a turn): no rate,
        // so we never flash a div-by-near-zero blowup.
        assert_eq!(live_tokens_per_sec(400, 0.0), None);
    }

    #[test]
    fn current_turn_streamed_counts_only_this_turn() {
        fn text_entry(
            id: &str,
            role: MessageRole,
            text: &str,
            streaming: bool,
        ) -> SessionMessageEntry {
            SessionMessageEntry {
                id: id.into(),
                role,
                parts: vec![MessagePart::Text {
                    id: format!("{id}-p"),
                    text: text.into(),
                }],
                created_at: 0,
                device_id: "d".into(),
                status: streaming.then_some(comet_doc::MessageStatus::Streaming),
                continuation_of: None,
            }
        }
        let mut state = AppState::new();
        state.transcript = vec![
            // Prior turn — must NOT be counted (it precedes the last user msg).
            text_entry("u0", MessageRole::User, "earlier prompt", false),
            text_entry(
                "a-early",
                MessageRole::Assistant,
                "earlier answer that is long, real long to make the point",
                false,
            ),
            // Current turn: this user message rolls the boundary forward.
            text_entry("u1", MessageRole::User, "build it", false),
            text_entry("a1", MessageRole::Assistant, "reasoning round", false),
            text_entry("a2", MessageRole::Assistant, "another round", false),
            text_entry("a3", MessageRole::Assistant, "the live burst", true),
            // A tool round: its JSON arguments are model-generated output tokens,
            // exactly what pi's real `output` counts — so they belong in the live
            // estimate too.
            SessionMessageEntry {
                id: "a4".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Tool {
                    id: "a4-t".into(),
                    call: comet_proto::ToolCall::ReadFile {
                        path: "/src/main.rs".into(),
                    },
                    is_error: false,
                    resolved: true,
                }],
                created_at: 0,
                device_id: "d".into(),
                status: None,
                continuation_of: None,
            },
        ];
        // Tool JSON is counted (exact serialized length), on top of the text.
        let tool_json = serde_json::to_string(&comet_proto::ToolCall::ReadFile {
            path: "/src/main.rs".into(),
        })
        .unwrap();
        let expected = ("reasoning round".chars().count()
            + "another round".chars().count()
            + "the live burst".chars().count()
            + tool_json.chars().count()) as u64;
        assert_eq!(
            state.current_turn_streamed(),
            expected,
            "tool rounds of the current turn count"
        );
    }

    #[test]
    fn chats_in_project_filters_and_orders() {
        let mut state = AppState::new();
        state.apply_projects(vec![project("s1", "dev", "/a", 1)]);
        let mut in_project_new = chat("new", 5, None);
        in_project_new.project_id = Some("s1".into());
        let mut in_project_old = chat("old", 1, Some(50)); // active but created first
        in_project_old.project_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.project_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.project_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no project id
        state.apply_chats(vec![
            in_project_new,
            in_project_old,
            other,
            archived,
            dangling,
        ]);
        let ids: Vec<&str> = state
            .chats_in_project("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        // The overview shows every live-project chat (idle included) — chats of
        // unknown projects stay hidden. Completed ("old") outranks idle ("new").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new"]);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = comet_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(comet_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            comet_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: comet_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn gate_phases() {
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into())),
            GatePhase::Failed("boom".into())
        );
        assert_eq!(gate_phase(&ConnectionStatus::Ready), GatePhase::Ready);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/comet")), "comet");
        assert_eq!(project_label(Some("/home/w/dev/comet/")), "comet");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/comet")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/comet")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["comet", "zed", "No project"]);
        let comet_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(comet_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_comet_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("comet/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · comet/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }
}
