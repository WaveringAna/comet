//! The terminal panel: session-scoped tabs over engine PTYs.
//!
//! Feature-inventory §1.10: tabs are per selected chat and restored on return
//! (emulators — and their server-side PTYs — survive navigation; detach is not
//! close). Tab bar supports pointer drag-reorder with 150 ms sliding
//! transforms, middle-click close, and a "+" new-tab button; Cmd/Ctrl+J
//! toggles the panel (the shell owns the height animation + persistence).
//!
//! Data path per tab: `OpenTerminal` → `SubscribeTerminal` stream; Data frames
//! (base64) feed the [`Emulator`]; query responses write back; the stream
//! reconnects with exponential backoff resuming from `afterSeq`; Exit appends
//! the "[process exited N]" line and stops. Keyboard bytes coalesce for 12 ms
//! before `WriteTerminal`; viewport-driven resizes debounce 80 ms before
//! `ResizeTerminal` (the emulator resizes immediately).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, IntoElement, KeyBinding, KeyDownEvent,
    MouseButton, Render, ScrollDelta, SharedString, Subscription, Task, Window, actions, div,
    prelude::*, px,
};

use comet_doc::MessagePart;
use comet_proto::view::single_line;
use comet_proto::{Chat, HarnessId, TerminalEvent, TerminalSession, ToolCall, ToolOutputReply};
use comet_rpc::methods;

use crate::motion::{self, AnimationExt as _, TAB_SLIDE};
use crate::settings::{TERMINAL_MAX_VH, TERMINAL_MIN_HEIGHT};
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

use super::emulator::{CellSnapshot, CursorSnapshot, Emulator};
use super::view::{
    COALESCE_MS, InputCoalescer, RESIZE_DEBOUNCE_MS, TerminalElement, keystroke_bytes, paste_bytes,
    terminal_bg,
};

/// Fixed tab width — drag-reorder math stays analytic.
pub const TAB_WIDTH: f32 = 118.0;
pub const TAB_BAR_HEIGHT: f32 = 40.0;

/// Agent-feed row geometry (uniform, so `scroll_to_item` anchoring is exact).
pub const FEED_ROW_HEIGHT: f32 = 20.0;
/// How long a deep-linked group's rows keep their flash wash.
pub const FEED_FLASH_MS: u64 = 1600;
/// How many output lines an expanded row shows (the tail — the last lines
/// are what you came for).
pub const FEED_OUTPUT_MAX_LINES: usize = 24;

// ---------------------------------------------------------------------------
// Agent command feed (the "<agent>'s terminal" tab — pure)
// ---------------------------------------------------------------------------

/// One command in the agent feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    /// The `MessagePart::Tool` id — the transcript deep-link anchors on it.
    pub id: String,
    pub command: String,
    pub status: FeedStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedStatus {
    /// No ToolResult yet AND the session is live — "the latest it's running".
    Running,
    Ok,
    Failed,
    /// No ToolResult and the session is gone (crashed harness / killed run).
    /// Paints like a finished row: never an eternal spinner (the same rule
    /// as the Working indicator's staleness gate).
    Unfinished,
}

/// Every shell command the agent ran, in transcript order — the agent
/// terminal's scrollback. Commands are single-lined like the chips were (a
/// literal newline would be a cursor move in this voice too). The full
/// history stays reachable: the feed scrolls, and transcript deep-links
/// anchor into it by tool id.
///
/// `live` is the session's staleness-gated indicator (`effective_indicator`
/// != None): an unresolved command only reads as running while the session
/// that launched it is actually alive.
pub fn exec_feed(entries: &[comet_doc::SessionMessageEntry], live: bool) -> Vec<FeedEntry> {
    entries
        .iter()
        .flat_map(|entry| entry.parts.iter())
        .filter_map(|part| match part {
            MessagePart::Tool {
                id,
                call: ToolCall::Exec { command },
                is_error,
                resolved,
            } => Some(FeedEntry {
                id: id.clone(),
                command: single_line(command),
                status: if *is_error {
                    FeedStatus::Failed
                } else if *resolved {
                    FeedStatus::Ok
                } else if live {
                    FeedStatus::Running
                } else {
                    FeedStatus::Unfinished
                },
            }),
            _ => None,
        })
        .collect()
}

/// The scroll target for this frame: a deep-link anchor wins; otherwise any
/// change to the tail fingerprint — `(feed_len, tail_expansion_lines)` —
/// scrolls the tail into view: a new command arrived, or the tail row's
/// expansion grew/shrank (output loaded, auto-collapse as the tail moved,
/// user toggled the last row). Follow-bottom without needing the scroll
/// offset; yanking on real tail movement is correct for a command feed.
pub fn follow_target(
    prev: (usize, usize),
    cur: (usize, usize),
    anchor: Option<usize>,
) -> Option<usize> {
    if let Some(ix) = anchor {
        return Some(ix);
    }
    (prev != cur && cur.0 > 0).then(|| cur.0 - 1)
}

/// How close to the tail the scroll offset must sit for follow-bottom to
/// yank (gpui offsets are negative going down; `max_offset` is the positive
/// clamp, so at the tail `offset == -max_offset`).
const FEED_FOLLOW_EPSILON: f32 = 2.0;

/// Whether the feed is currently pinned to its tail — the xterm gate: a tail
/// fingerprint change only yanks while this holds, so the user can scroll
/// back through history during a live session without the next command
/// dragging them down. Scrolling back to the bottom re-engages following.
pub fn feed_at_bottom(offset_y: f32, max_offset_y: f32) -> bool {
    max_offset_y + offset_y <= FEED_FOLLOW_EPSILON
}

/// Effective expansion for a feed row: the LATEST command auto-expands
/// (older ones collapse as the tail moves) — a manual pin keeps a row open
/// regardless, a manual dismissal keeps even the latest row shut.
pub fn is_feed_row_expanded(
    pinned: Option<&HashSet<String>>,
    dismissed: Option<&HashSet<String>>,
    id: &str,
    is_last: bool,
) -> bool {
    pinned.is_some_and(|s| s.contains(id))
        || (is_last && !dismissed.is_some_and(|s| s.contains(id)))
}

/// Harness label for the tab title (brand voice: lowercase for pi).
pub fn harness_label(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Pi => "pi",
        HarnessId::ClaudeCode => "claude code",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor",
        HarnessId::Mock => "mock",
    }
}

/// Provider prefix noise ("anthropic/claude-opus-4.5" → "claude-opus-4.5").
pub fn short_model_name(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// The name the pinned tab is titled after: the chat's model (provider
/// prefix stripped), falling back to the harness name, then "agent" before
/// config lands.
pub fn agent_tab_name(config: Option<&comet_proto::ChatConfig>) -> &str {
    config
        .map(|config| {
            config
                .model
                .as_deref()
                .map(short_model_name)
                .unwrap_or_else(|| harness_label(config.harness))
        })
        .unwrap_or("agent")
}

/// The pinned tab's title: "<model or harness>'s terminal".
pub fn agent_tab_title(chat: Option<&Chat>) -> SharedString {
    SharedString::from(format!(
        "{}'s terminal",
        agent_tab_name(chat.and_then(|c| c.config.as_ref()))
    ))
}

actions!(terminal, [ToggleTerminal]);

/// Bind the terminal keymap (global): Cmd+J on macOS, Ctrl+J elsewhere.
pub fn init(cx: &mut App) {
    let toggle = if cfg!(target_os = "macos") {
        "cmd-j"
    } else {
        "ctrl-j"
    };
    cx.bind_keys([KeyBinding::new(toggle, ToggleTerminal, None)]);
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Panel height clamp: 160 px … 55 % of the viewport (§1.10).
pub fn clamp_terminal_height(height: f32, viewport_h: f32) -> f32 {
    let max = (viewport_h * TERMINAL_MAX_VH).max(TERMINAL_MIN_HEIGHT);
    if height.is_finite() {
        height.clamp(TERMINAL_MIN_HEIGHT, max)
    } else {
        TERMINAL_MIN_HEIGHT
    }
}

/// Reconnect backoff: 500 ms doubling to an 8 s ceiling.
pub fn backoff_ms(attempt: u32) -> u64 {
    (500u64 << attempt.min(4)).min(8_000)
}

/// Move a tab from `from` to `to` (indices into the same vec).
pub fn reorder_tabs<T>(tabs: &mut Vec<T>, from: usize, to: usize) {
    if from >= tabs.len() || to >= tabs.len() || from == to {
        return;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
}

/// Where a drag hovering at `rel_x` inside the tab strip would land.
pub fn drop_index(rel_x: f32, tab_w: f32, count: usize) -> usize {
    if count == 0 || tab_w <= 0.0 {
        return 0;
    }
    ((rel_x / tab_w).floor().max(0.0) as usize).min(count - 1)
}

/// Sliding transform (in tab-width units) for tab `ix` while `from` is dragged
/// over `over`: tabs between the two shift one slot toward the vacated gap.
pub fn slide_offset(ix: usize, from: usize, over: usize) -> f32 {
    if from < over && ix > from && ix <= over {
        -1.0
    } else if over < from && ix >= over && ix < from {
        1.0
    } else {
        0.0
    }
}

/// Active index after a reorder commit.
pub fn active_after_reorder(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// Merge the `targetDeviceId` passthrough into RPC params (no-op for chats on
/// the connected engine's own device).
fn with_target(mut params: serde_json::Value, target: &Option<String>) -> serde_json::Value {
    if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(target.clone()),
        );
    }
    params
}

/// Active index after closing `closed` (given the new, shorter length).
pub fn active_after_close(active: usize, closed: usize, len_after: usize) -> usize {
    let shifted = if closed < active { active - 1 } else { active };
    if len_after == 0 {
        0
    } else {
        shifted.min(len_after - 1)
    }
}

/// The `[process exited N]` trailer, dimmed (§1.10).
pub fn exit_message(code: i32) -> Vec<u8> {
    format!("\r\n\x1b[90m[process exited {code}]\x1b[0m\r\n").into_bytes()
}

/// "Terminal N" numbering counts PTY tabs only — the pinned agent tab at
/// slot 0 isn't "Terminal 1".
fn next_pty_number(kinds: impl Iterator<Item = TabKind>) -> usize {
    kinds.filter(|kind| *kind == TabKind::Pty).count() + 1
}

/// Tab title from the session's shell path ("/bin/zsh" → "zsh").
pub fn shell_title(shell: &str) -> String {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell).trim();
    if name.is_empty() {
        "terminal".to_string()
    } else {
        name.to_string()
    }
}

fn decode_base64(data: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "terminal: dropping undecodable data frame");
            Vec::new()
        })
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// A grid snapshot handed to the paint element.
pub struct GridSnapshot {
    pub lines: Vec<Vec<CellSnapshot>>,
    pub cursor: Option<CursorSnapshot>,
}

/// What a tab shows: a real engine PTY, or the agent's command feed (the
/// pinned first tab — "<agent>'s terminal": the commands the model runs live
/// here, not in transcript chips).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabKind {
    Pty,
    Agent,
}

struct TerminalTab {
    key: u64,
    kind: TabKind,
    title: SharedString,
    terminal_id: Option<String>,
    emulator: Emulator,
    exited: Option<i32>,
    last_seq: u64,
    coalescer: InputCoalescer,
    flush_task: Option<Task<()>>,
    resize_task: Option<Task<()>>,
    /// Open + subscribe/reconnect lifecycle; dropping it cancels the stream.
    _run: Option<Task<()>>,
}

/// A fetched `ToolOutput` answer, cached per chat. `Unavailable` is cached
/// too — for a resolved command "no record" is a stable answer, and re-
/// fetching on every expand would just hammer the journal.
enum FeedOutput {
    Loaded { output: String, truncated: bool },
    Unavailable,
}

/// What an expanded feed row shows under the command line.
enum FeedExpansion<'a> {
    Collapsed,
    /// Expanded while the command is still unresolved — no fetch yet (the
    /// journal has nothing until the ToolResult lands).
    StillRunning,
    Loading,
    Lines(&'a [String]),
    Unavailable,
    /// Resolved with an explicitly empty capture.
    NoOutput,
}

/// Expanded-row display lines: the LAST [`FEED_OUTPUT_MAX_LINES`] of the
/// capture, with marker lines for what got cut (at capture time and/or by
/// the view cap).
pub fn output_display_lines(output: &str, truncated: bool) -> Vec<String> {
    let all: Vec<&str> = output.lines().collect();
    let hidden = all.len().saturating_sub(FEED_OUTPUT_MAX_LINES);
    let mut out = Vec::with_capacity(all.len() - hidden + 2);
    if truncated {
        out.push(format!(
            "… truncated to the last {} KB",
            comet_proto::TOOL_OUTPUT_MAX_BYTES / 1024
        ));
    }
    if hidden > 0 {
        out.push(format!("… {hidden} earlier lines"));
    }
    out.extend(all.iter().skip(hidden).map(|line| line.to_string()));
    out
}

#[derive(Default)]
pub struct ChatTabs {
    tabs: Vec<TerminalTab>,
    active: usize,
    /// Manually expanded feed rows (pins — survive the auto tail moving past).
    feed_pinned: HashSet<String>,
    /// Manually collapsed rows — a dismissal keeps even the latest row shut.
    feed_dismissed: HashSet<String>,
    /// `ToolOutput` answers by tool id.
    feed_outputs: HashMap<String, FeedOutput>,
    /// In-flight `ToolOutput` calls (kills render-loop refetch).
    feed_pending: HashSet<String>,
}

/// Drag-reorder state; `epoch` keys the 150 ms slide animation restarts.
struct DragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-tab payload (gpui drag-and-drop).
struct TabDragPayload {
    chat: String,
    from: usize,
    title: SharedString,
}

struct TabGhost {
    title: SharedString,
}

impl Render for TabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(TAB_WIDTH))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .rounded(px(Theme::CONTROL_RADIUS))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(12.0))
            .text_color(theme.text)
            .opacity(0.85)
            .child(div().truncate().child(self.title.clone()))
    }
}

pub struct TerminalPanel {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    chats: HashMap<String, ChatTabs>,
    /// Shell-driven visibility gate: no RPC happens while closed (lazy).
    open: bool,
    tab_seq: u64,
    drag: Option<DragState>,
    last_selected: Option<String>,
    /// Agent-feed scroll; reset on chat switch so the new chat's first frame
    /// follows to its tail (offset/max start zeroed = "at bottom").
    feed_scroll: gpui::ScrollHandle,
    /// Deep-link anchor: the tool id to scroll into view (consumed on render).
    pending_anchor: Option<String>,
    /// Rows flashing from the last deep link (cleared on a timer).
    flash_ids: Vec<String>,
    /// Follow-bottom baseline: the `(feed_len, tail_expansion_lines)`
    /// fingerprint at the last render — any change scrolls the tail into view.
    last_tail_fp: (usize, usize),
    _observe: Subscription,
}

impl TerminalPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        Self {
            state,
            focus_handle: cx.focus_handle(),
            chats: HashMap::new(),
            open: false,
            tab_seq: 0,
            drag: None,
            last_selected: None,
            feed_scroll: gpui::ScrollHandle::new(),
            pending_anchor: None,
            flash_ids: Vec::new(),
            last_tail_fp: (0, 0),
            _observe: observe,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Shell toggle hook. Opening lazily creates the first tab for the
    /// selected chat; closing keeps every session alive (detach ≠ close).
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if open {
            self.ensure_tab(cx);
        }
        cx.notify();
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.read(cx).selected_chat.clone();
        let switched = selected != self.last_selected;
        if switched {
            self.last_selected = selected;
            self.drag = None;
            // The feed belongs to the old chat: a fresh scroll handle (the
            // carried offset would otherwise read "not at bottom" and the
            // at-bottom gate would lock the new chat's follow), a re-baselined
            // fingerprint so the first frame follows to the tail, and no
            // deep-link state aimed at rows that are no longer shown.
            self.pending_anchor = None;
            self.flash_ids.clear();
            self.feed_scroll = gpui::ScrollHandle::new();
            self.last_tail_fp = (0, 0);
        }
        if self.open {
            // Returning to a chat with tabs restores them; a fresh chat (or an
            // engine that only just finished booting) gets its first tab —
            // ensure_tab is idempotent, so calling on every state change is safe.
            self.ensure_tab(cx);
        }
        // A visible agent feed re-renders on every state change (transcript
        // frames stream in at 120ms commits); PTY tabs only need chat swaps.
        if switched || (self.open && self.active_is_agent(cx)) {
            cx.notify();
        }
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// The chat's host device when it differs from the connected engine's own —
    /// the PTY lives on the chat's device (feature-inventory §2.1 "terminals
    /// live on the chat's host device"), so every terminal RPC for a remote
    /// chat needs the `targetDeviceId` passthrough. Without it the local
    /// engine checks the chat's cwd against its OWN filesystem and fails with
    /// "Session working directory is unavailable" (user report).
    fn chat_target(&self, chat: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state
            .chats
            .iter()
            .find(|c| c.id == chat)?
            .device_id
            .clone();
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    fn selected_chat(&self, cx: &App) -> Option<String> {
        self.state.read(cx).selected_chat.clone()
    }

    fn ensure_tab(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        self.ensure_agent_tab(&chat, cx);
        if self
            .chats
            .get(&chat)
            .is_none_or(|c| !c.tabs.iter().any(|t| t.kind == TabKind::Pty))
        {
            self.open_tab(chat, cx);
        }
    }

    /// The pinned agent-feed tab at slot 0 (created once per chat; never
    /// closable, never draggable). Inserting it shifts any restored active
    /// index one slot right.
    fn ensure_agent_tab(&mut self, chat: &str, cx: &mut Context<Self>) {
        if self
            .chats
            .get(chat)
            .is_some_and(|c| c.tabs.first().is_some_and(|t| t.kind == TabKind::Agent))
        {
            return;
        }
        self.tab_seq += 1;
        let key = self.tab_seq;
        let entry = self.chats.entry(chat.to_string()).or_default();
        entry.tabs.insert(
            0,
            TerminalTab {
                key,
                kind: TabKind::Agent,
                title: SharedString::default(),
                terminal_id: None,
                emulator: Emulator::new(80, 24),
                exited: None,
                last_seq: 0,
                coalescer: InputCoalescer::default(),
                flush_task: None,
                resize_task: None,
                _run: None,
            },
        );
        if entry.tabs.len() > 1 {
            entry.active += 1;
        }
        cx.notify();
    }

    fn active_is_agent(&self, cx: &App) -> bool {
        let Some(chat) = self.state.read(cx).selected_chat.clone() else {
            return false;
        };
        self.chats
            .get(&chat)
            .is_some_and(|tabs| {
                tabs.tabs
                    .get(tabs.active)
                    .is_some_and(|t| t.kind == TabKind::Agent)
            })
    }

    fn tab_mut(&mut self, chat: &str, key: u64) -> Option<&mut TerminalTab> {
        self.chats
            .get_mut(chat)?
            .tabs
            .iter_mut()
            .find(|t| t.key == key)
    }

    fn active_tab(&self, cx: &App) -> Option<&TerminalTab> {
        let chat = self.state.read(cx).selected_chat.clone()?;
        let tabs = self.chats.get(&chat)?;
        tabs.tabs.get(tabs.active)
    }

    // ---- open / stream lifecycle ----

    fn open_tab(&mut self, chat: String, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.tab_seq += 1;
        let key = self.tab_seq;
        let entry = self.chats.entry(chat.clone()).or_default();
        // Numbering counts PTY tabs only — the pinned agent tab at slot 0
        // isn't "Terminal 1".
        let tab_no = next_pty_number(entry.tabs.iter().map(|t| t.kind));
        entry.tabs.push(TerminalTab {
            key,
            kind: TabKind::Pty,
            title: format!("Terminal {tab_no}").into(),
            terminal_id: None,
            emulator: Emulator::new(80, 24),
            exited: None,
            last_seq: 0,
            coalescer: InputCoalescer::default(),
            flush_task: None,
            resize_task: None,
            _run: None,
        });
        entry.active = entry.tabs.len() - 1;

        let target = self.chat_target(&chat, cx);
        let run = Self::spawn_session(chat.clone(), key, engine, target, cx);
        if let Some(tab) = self.tab_mut(&chat, key) {
            tab._run = Some(run);
        }
        cx.notify();
    }

    /// OpenTerminal, then pump SubscribeTerminal with reconnect backoff.
    fn spawn_session(
        chat: String,
        key: u64,
        engine: EngineHandle,
        target: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let (cols, rows) = this
                .update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.emulator.cols() as u16, t.emulator.rows() as u16))
                        .unwrap_or((80, 24))
                })
                .unwrap_or((80, 24));

            let opened = engine
                .client()
                .call_as::<TerminalSession>(
                    methods::OPEN_TERMINAL,
                    with_target(
                        serde_json::json!({ "chatId": chat, "cols": cols, "rows": rows }),
                        &target,
                    ),
                )
                .await;
            let session = match opened {
                Ok(session) => session,
                Err(err) => {
                    tracing::warn!(error = %err, "OpenTerminal failed");
                    let _ = this.update(cx, |panel, cx| {
                        if let Some(tab) = panel.tab_mut(&chat, key) {
                            tab.emulator.feed(
                                format!("\x1b[31mfailed to open terminal: {err}\x1b[0m\r\n")
                                    .as_bytes(),
                            );
                            tab.exited = Some(-1);
                            cx.notify();
                        }
                    });
                    return;
                }
            };
            let terminal_id = session.id.clone();
            let attached = this
                .update(cx, |panel, cx| {
                    if let Some(tab) = panel.tab_mut(&chat, key) {
                        tab.terminal_id = Some(terminal_id.clone());
                        cx.notify();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if !attached {
                // Tab was closed before the open completed — release the PTY.
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id }),
                            &target,
                        ),
                    )
                    .await;
                return;
            }

            let mut attempt: u32 = 0;
            loop {
                let Ok(after_seq) = this.update(cx, |panel, _| {
                    panel.tab_mut(&chat, key).map(|t| t.last_seq)
                }) else {
                    return; // entity released
                };
                let Some(after_seq) = after_seq else { return }; // tab closed

                let subscribed = engine
                    .client()
                    .subscribe(
                        methods::SUBSCRIBE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": terminal_id, "afterSeq": after_seq }),
                            &target,
                        ),
                    )
                    .await;
                let mut rx = match subscribed {
                    Ok(rx) => rx,
                    Err(err) => {
                        tracing::debug!(error = %err, attempt, "SubscribeTerminal failed; backing off");
                        cx.background_executor()
                            .timer(Duration::from_millis(backoff_ms(attempt)))
                            .await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                };

                while let Some(value) = rx.recv().await {
                    let event: TerminalEvent = match serde_json::from_value(value) {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!(error = %err, "terminal: malformed stream frame");
                            continue;
                        }
                    };
                    attempt = 0;
                    let outcome = this.update(cx, |panel, cx| {
                        panel.apply_stream_event(&chat, key, &engine, event, cx)
                    });
                    match outcome {
                        Ok(StreamDisposition::Continue) => {}
                        Ok(StreamDisposition::Stop) => return,
                        Err(_) => return,
                    }
                }

                // Stream dropped without an exit — reconnect from afterSeq.
                let done = this
                    .update(cx, |panel, _| {
                        panel.tab_mut(&chat, key).map(|t| t.exited.is_some()).unwrap_or(true)
                    })
                    .unwrap_or(true);
                if done {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(backoff_ms(attempt)))
                    .await;
                attempt = attempt.saturating_add(1);
            }
        })
    }

    fn apply_stream_event(
        &mut self,
        chat: &str,
        key: u64,
        engine: &EngineHandle,
        event: TerminalEvent,
        cx: &mut Context<Self>,
    ) -> StreamDisposition {
        let target = self.chat_target(chat, cx);
        let Some(tab) = self.tab_mut(chat, key) else {
            return StreamDisposition::Stop;
        };
        match event {
            TerminalEvent::Data { seq, data } => {
                tab.last_seq = seq;
                let responses = tab.emulator.feed(&decode_base64(&data));
                if !responses.is_empty()
                    && let Some(id) = tab.terminal_id.clone()
                {
                    // Query responses (DSR etc.) go straight back, no coalescing.
                    let engine = engine.clone();
                    let data = encode_base64(&responses);
                    cx.spawn(async move |_, _| {
                        let _ = engine
                            .client()
                            .call(
                                methods::WRITE_TERMINAL,
                                with_target(
                                    serde_json::json!({ "terminalId": id, "data": data }),
                                    &target,
                                ),
                            )
                            .await;
                    })
                    .detach();
                }
                cx.notify();
                StreamDisposition::Continue
            }
            TerminalEvent::Exit { seq, exit_code, .. } => {
                tab.last_seq = seq;
                tab.exited = Some(exit_code);
                tab.emulator.feed(&exit_message(exit_code));
                cx.notify();
                StreamDisposition::Stop
            }
        }
    }

    // ---- input ----

    /// Queue keyboard bytes on the active tab (12 ms coalescing window).
    fn queue_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.exited.is_some() {
            return;
        }
        // A keypress while scrolled back snaps to the live bottom (xterm).
        if tab.emulator.display_offset() > 0 {
            tab.emulator.scroll_to_bottom();
        }
        let key = tab.key;
        if tab.coalescer.push(bytes) {
            tab.flush_task = Some(Self::schedule_flush(chat, key, cx));
        }
    }

    fn schedule_flush(chat: String, key: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(COALESCE_MS))
                .await;
            let _ = this.update(cx, |panel, cx| panel.flush_input(chat, key, cx));
        })
    }

    fn flush_input(&mut self, chat: String, key: u64, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.chat_target(&chat, cx);
        let Some(tab) = self.tab_mut(&chat, key) else {
            return;
        };
        if tab.coalescer.is_empty() {
            return;
        }
        let Some(id) = tab.terminal_id.clone() else {
            // OpenTerminal still in flight — keep the buffer, retry shortly.
            if tab.exited.is_none() {
                tab.flush_task = Some(Self::schedule_flush(chat, key, cx));
            }
            return;
        };
        let data = encode_base64(&tab.coalescer.take());
        cx.spawn(async move |_, _| {
            let _ = engine
                .client()
                .call(
                    methods::WRITE_TERMINAL,
                    with_target(
                        serde_json::json!({ "terminalId": id, "data": data }),
                        &target,
                    ),
                )
                .await;
        })
        .detach();
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self
            .active_tab(cx)
            .map(|tab| tab.emulator.bracketed_paste_mode())
            .unwrap_or(false);
        let bytes = paste_bytes(&text, bracketed);
        self.queue_input(&bytes, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        // Paste: Cmd+V (macOS) / Ctrl+Shift+V.
        if ks.key == "v" && (mods.platform || (mods.control && mods.shift)) {
            self.paste_clipboard(cx);
            cx.stop_propagation();
            return;
        }
        let app_cursor = self
            .active_tab(cx)
            .map(|tab| tab.emulator.app_cursor_mode())
            .unwrap_or(false);
        if let Some(bytes) = keystroke_bytes(&ks.key, ks.key_char.as_deref(), mods, app_cursor) {
            self.queue_input(&bytes, cx);
            cx.stop_propagation();
        }
    }

    // ---- grid metrics / element hooks ----

    /// Called from element prepaint with the measured cols×rows. Resizes the
    /// emulator immediately; the `ResizeTerminal` RPC debounces 80 ms.
    pub fn on_grid_metrics(&mut self, cols: u16, rows: u16, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        let Some(tab) = tabs.tabs.get_mut(active) else {
            return;
        };
        if tab.emulator.cols() == cols as usize && tab.emulator.rows() == rows as usize {
            return;
        }
        tab.emulator.resize(cols, rows);
        let key = tab.key;
        let engine = self.engine(cx);
        let target = self.chat_target(&chat, cx);
        if let (Some(engine), Some(tab)) = (engine, self.tab_mut(&chat, key)) {
            let id = tab.terminal_id.clone();
            tab.resize_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_DEBOUNCE_MS))
                    .await;
                // Re-read the *current* size — later prepaints may have
                // resized again inside the debounce window.
                let Ok(current) = this.update(cx, |panel, _| {
                    panel
                        .tab_mut(&chat, key)
                        .map(|t| (t.terminal_id.clone(), t.emulator.cols(), t.emulator.rows()))
                }) else {
                    return;
                };
                let Some((stored_id, cols, rows)) = current else {
                    return;
                };
                let Some(id) = stored_id.or(id) else { return };
                let _ = engine
                    .client()
                    .call(
                        methods::RESIZE_TERMINAL,
                        with_target(
                            serde_json::json!({ "terminalId": id, "cols": cols, "rows": rows }),
                            &target,
                        ),
                    )
                    .await;
            }));
        }
        // Deliberately no cx.notify(): this runs during prepaint of the
        // current frame, which already paints the resized grid.
    }

    /// Snapshot for the paint element.
    pub fn active_grid_snapshot(&self, cx: &App) -> Option<GridSnapshot> {
        let tab = self.active_tab(cx)?;
        Some(GridSnapshot {
            lines: tab.emulator.lines(),
            cursor: tab.emulator.cursor(),
        })
    }

    fn scroll_active(&mut self, delta_lines: i32, cx: &mut Context<Self>) {
        if delta_lines == 0 {
            return;
        }
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        let Some(tabs) = self.chats.get_mut(&chat) else {
            return;
        };
        let active = tabs.active;
        if let Some(tab) = tabs.tabs.get_mut(active) {
            tab.emulator.scroll(delta_lines);
            cx.notify();
        }
    }

    // ---- tab management ----

    fn select_tab(&mut self, chat: &str, ix: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat)
            && ix < tabs.tabs.len()
        {
            tabs.active = ix;
            cx.notify();
        }
    }

    /// Deep-link from the transcript's "ran N commands" pill: pin the agent
    /// tab active, anchor the feed at the group's first command, and flash
    /// the group's rows so the commands you came from are findable.
    pub fn reveal_agent_commands(&mut self, tool_ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(chat) = self.selected_chat(cx) else {
            return;
        };
        self.ensure_agent_tab(&chat, cx);
        if let Some(tabs) = self.chats.get_mut(&chat)
            && !tabs.tabs.is_empty()
        {
            tabs.active = 0;
        }
        self.pending_anchor = tool_ids.first().cloned();
        self.flash_ids = tool_ids;
        let flash_clear = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(FEED_FLASH_MS))
                .await;
            this.update(cx, |panel, cx| {
                panel.flash_ids.clear();
                cx.notify();
            })
            .ok();
        });
        flash_clear.detach();
        cx.notify();
    }

    /// Row click: flip a feed row's expansion. Effective expansion is
    /// `is_feed_row_expanded` — the LATEST row auto-expands and older ones
    /// auto-collapse, with manual pins/dismissals overriding. The fetch
    /// itself is render-driven (`ensure_feed_fetch`) so a row expanded
    /// while Running picks up its output the frame the command resolves.
    fn toggle_feed_row(&mut self, chat: &str, tool_id: &str, cx: &mut Context<Self>) {
        let is_last = {
            let state = self.state.read(cx);
            exec_feed(&state.transcript, true)
                .last()
                .is_some_and(|entry| entry.id == tool_id)
        };
        let entry = self.chats.entry(chat.to_owned()).or_default();
        if is_feed_row_expanded(
            Some(&entry.feed_pinned),
            Some(&entry.feed_dismissed),
            tool_id,
            is_last,
        ) {
            entry.feed_pinned.remove(tool_id);
            entry.feed_dismissed.insert(tool_id.to_owned());
        } else {
            entry.feed_dismissed.remove(tool_id);
            entry.feed_pinned.insert(tool_id.to_owned());
        }
        cx.notify();
    }

    /// Kick a `ToolOutput` call for an expanded, resolved, uncached row.
    /// Host-local lookup: the answer comes from the chat host's run journal
    /// (`target` routes there), never from the synced doc.
    fn ensure_feed_fetch(&mut self, chat: &str, tool_id: &str, status: FeedStatus, cx: &mut Context<Self>) {
        if status == FeedStatus::Running {
            return;
        }
        let needs_fetch = {
            let entry = self.chats.entry(chat.to_owned()).or_default();
            !entry.feed_outputs.contains_key(tool_id) && entry.feed_pending.insert(tool_id.to_owned())
        };
        if !needs_fetch {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            if let Some(entry) = self.chats.get_mut(chat) {
                entry.feed_pending.remove(tool_id);
                entry.feed_outputs.insert(tool_id.to_owned(), FeedOutput::Unavailable);
            }
            return;
        };
        let target = self.chat_target(chat, cx);
        let chat_owned = chat.to_owned();
        let tool_owned = tool_id.to_owned();
        cx.spawn(async move |this, cx| {
            let reply = engine
                .client()
                .call_as::<ToolOutputReply>(
                    methods::TOOL_OUTPUT,
                    with_target(
                        serde_json::json!({ "chatId": chat_owned, "toolId": tool_owned }),
                        &target,
                    ),
                )
                .await;
            this.update(cx, |panel, cx| {
                if let Some(entry) = panel.chats.get_mut(&chat_owned) {
                    entry.feed_pending.remove(&tool_owned);
                    if let Ok(reply) = reply {
                        // found:false / output:None are stable answers for a
                        // resolved command — cache them.
                        let output = match (reply.found, reply.output) {
                            (true, Some(output)) => FeedOutput::Loaded {
                                output,
                                truncated: reply.truncated,
                            },
                            _ => FeedOutput::Unavailable,
                        };
                        entry.feed_outputs.insert(tool_owned, output);
                        cx.notify();
                    }
                    // Transient RPC/relay failure (Err): left UNCACHED and no
                    // notify, or render would refire in a hot loop — the next
                    // natural render retries.
                }
            })
            .ok();
        })
        .detach();
    }

    fn close_tab(&mut self, chat: &str, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let engine = self.engine(cx);
        let target = self.chat_target(chat, cx);
        let Some(tabs) = self.chats.get_mut(chat) else {
            return;
        };
        let Some(ix) = tabs.tabs.iter().position(|t| t.key == key) else {
            return;
        };
        // The pinned agent tab is not closable (no close button either, but
        // middle-click still reaches here).
        if tabs.tabs[ix].kind == TabKind::Agent {
            return;
        }
        let tab = tabs.tabs.remove(ix);
        tabs.active = active_after_close(tabs.active, ix, tabs.tabs.len());
        // Closing the LAST PTY closes the drawer too — a dock showing only
        // the feed wasn't asked for by a terminal close (comet parity: the
        // empty-dock rule, with the agent tab not counting as a terminal).
        let now_empty = !tabs.tabs.iter().any(|t| t.kind == TabKind::Pty);
        self.drag = None;
        // Closing the LAST terminal closes the drawer too — an empty dock is
        // dead space (user request). Same path as the collapse chevron.
        if now_empty && self.open {
            window.dispatch_action(Box::new(ToggleTerminal), cx);
        }
        if let (Some(engine), Some(id)) = (engine, tab.terminal_id.clone()) {
            cx.spawn(async move |_, _| {
                let _ = engine
                    .client()
                    .call(
                        methods::CLOSE_TERMINAL,
                        with_target(serde_json::json!({ "terminalId": id }), &target),
                    )
                    .await;
            })
            .detach();
        }
        cx.notify();
    }

    fn commit_reorder(&mut self, chat: &str, from: usize, to: usize, cx: &mut Context<Self>) {
        if let Some(tabs) = self.chats.get_mut(chat) {
            let active = tabs.active;
            reorder_tabs(&mut tabs.tabs, from, to);
            tabs.active = active_after_reorder(active, from, to);
        }
        self.drag = None;
        cx.notify();
    }

    fn update_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.drag {
            Some(drag) if drag.over != over => {
                drag.prev_over = drag.over;
                drag.over = over;
                drag.epoch += 1;
                cx.notify();
            }
            Some(_) => {}
            None => {
                self.drag = Some(DragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    // ---- render ----

    /// The agent tab's body: every command the agent ran, terminal-styled
    /// (mono, `$` prompts, the running one spinning), scrollable through the
    /// full history. Deep-link anchors scroll to the group's first command;
    /// its rows flash briefly.
    fn render_agent_feed(&mut self, chat: &str, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let feed = {
            let state = self.state.read(cx);
            let live = comet_proto::view::effective_indicator(
                state.session_for(chat),
                chrono::Utc::now(),
            ) != comet_proto::view::Indicator::None;
            exec_feed(&state.transcript, live)
        };

        // Scroll target for this frame: a pending deep-link anchor wins, else
        // any tail-fingerprint change (new command, or the tail row's
        // expansion growing/shrinking) follows to the tail. Pure rule:
        // `follow_target`. Rows are uniform-height direct children (expansion
        // blocks are measured), so `scroll_to_item` lands.
        let anchor_ix = self
            .pending_anchor
            .take()
            .and_then(|anchor| feed.iter().position(|e| e.id == anchor));
        // Tail fingerprint without allocating the display lines: mirror
        // `output_display_lines`' counting.
        let cur_fp = {
            let tabs = self.chats.get(chat);
            let tail_lines = match feed.last() {
                Some(tail)
                    if is_feed_row_expanded(
                        tabs.map(|t| &t.feed_pinned),
                        tabs.map(|t| &t.feed_dismissed),
                        &tail.id,
                        true,
                    ) =>
                {
                    if tail.status == FeedStatus::Running {
                        1
                    } else {
                        match tabs.and_then(|t| t.feed_outputs.get(&tail.id)) {
                            Some(FeedOutput::Loaded { output, truncated })
                                if !output.is_empty() =>
                            {
                                let total = output.lines().count();
                                let hidden = total.saturating_sub(FEED_OUTPUT_MAX_LINES);
                                total.min(FEED_OUTPUT_MAX_LINES)
                                    + usize::from(hidden > 0)
                                    + usize::from(*truncated)
                            }
                            // Loaded-empty / unavailable / in-flight all show
                            // a single note line.
                            _ => 1,
                        }
                    }
                }
                _ => 0,
            };
            (feed.len(), tail_lines)
        };
        if let Some(ix) = follow_target(self.last_tail_fp, cur_fp, anchor_ix) {
            if anchor_ix.is_some() {
                // Deep links always land, wherever the user scrolled.
                self.feed_scroll.scroll_to_item(ix);
            } else if feed_at_bottom(
                self.feed_scroll.offset().y.into(),
                self.feed_scroll.max_offset().y.into(),
            ) {
                // Tail-follow only while pinned to the tail — one-shot
                // `scroll_to_bottom` (consumed at paint, includes the tail
                // row's expansion); `scroll_to_item` would re-apply every
                // prepaint until resolvable and fight the user's wheel.
                self.feed_scroll.scroll_to_bottom();
            }
        }
        self.last_tail_fp = cur_fp;

        if feed.is_empty() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("no commands yet"))
                .into_any_element();
        }

        // Owned snapshot: a `&str` set would borrow self across the mutable
        // fetch kicks below.
        let flash: std::collections::HashSet<String> =
            self.flash_ids.iter().cloned().collect();
        // Fetch kick for expanded, resolved, uncached rows — render-driven so
        // a row expanded mid-run fetches on the frame its command resolves
        // (the doc flip re-renders us; `feed_pending` kills the refire).
        let feed_len = feed.len();
        for (ix, entry) in feed.iter().enumerate() {
            let tabs = self.chats.get(chat);
            let expanded = is_feed_row_expanded(
                tabs.map(|t| &t.feed_pinned),
                tabs.map(|t| &t.feed_dismissed),
                &entry.id,
                ix == feed_len - 1,
            );
            if expanded {
                self.ensure_feed_fetch(chat, &entry.id, entry.status, cx);
            }
        }
        let chat_owned = chat.to_owned();
        div()
            .id("agent-feed-scroll")
            // The scroller takes the (definite) body height — `flex_1` here
            // would grow it to content instead, overflowing its parent and
            // leaving max_offset at 0 (no scrollable range). Same pattern as
            // the sidebar scroller in shell.rs.
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.feed_scroll)
            .p(px(super::view::TERM_PADDING))
            .flex()
            .flex_col()
            .children(feed.iter().enumerate().map(|(ix, entry)| {
                let tabs = self.chats.get(chat);
                let expanded = is_feed_row_expanded(
                    tabs.map(|t| &t.feed_pinned),
                    tabs.map(|t| &t.feed_dismissed),
                    &entry.id,
                    ix == feed_len - 1,
                );
                let cached = tabs.and_then(|tabs| tabs.feed_outputs.get(&entry.id));
                let lines = match cached {
                    Some(FeedOutput::Loaded { output, truncated }) if !output.is_empty() => {
                        Some(output_display_lines(output, *truncated))
                    }
                    _ => None,
                };
                let expansion = if !expanded {
                    FeedExpansion::Collapsed
                } else if entry.status == FeedStatus::Running {
                    FeedExpansion::StillRunning
                } else {
                    match (cached, &lines) {
                        (Some(FeedOutput::Loaded { .. }), Some(lines)) => {
                            FeedExpansion::Lines(lines.as_slice())
                        }
                        (Some(FeedOutput::Loaded { .. }), None) => FeedExpansion::NoOutput,
                        (Some(FeedOutput::Unavailable), _) => FeedExpansion::Unavailable,
                        (None, _) => FeedExpansion::Loading,
                    }
                };
                let row = feed_row(entry, flash.contains(entry.id.as_str()), &expansion, theme);
                let tool_id = entry.id.clone();
                let chat_for_click = chat_owned.clone();
                row.id(SharedString::from(format!("feed-row-{}", entry.id)))
                    .cursor_pointer()
                    .hover(|el| el.bg(crate::theme::white_alpha(0.03)))
                    .on_click(cx.listener(
                        move |this, _: &gpui::ClickEvent, _window, cx| {
                            this.toggle_feed_row(&chat_for_click, &tool_id, cx);
                        },
                    ))
            }))
            .into_any_element()
    }

    fn render_tab_bar(&mut self, chat: &str, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = Theme::of(cx).clone();
        let tabs = self.chats.get(chat);
        let (active, count) = tabs.map(|t| (t.active, t.tabs.len())).unwrap_or((0, 0));
        let drag = self
            .drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));
        let chat_owned = chat.to_string();
        // Live title + running indicator for the pinned agent tab. Running
        // rides the same staleness gate as the Working indicator — a dead
        // session never spins forever.
        let state = self.state.read(cx);
        let chat_row = state.chats.iter().find(|c| c.id == chat);
        let agent_title = agent_tab_title(chat_row);
        let agent_harness = chat_row
            .and_then(|c| c.config.as_ref())
            .map(|config| config.harness)
            .unwrap_or(HarnessId::Pi);
        let session_live = comet_proto::view::effective_indicator(
            state.session_for(chat),
            chrono::Utc::now(),
        ) != comet_proto::view::Indicator::None;
        let agent_running = exec_feed(&state.transcript, session_live)
            .iter()
            .any(|e| e.status == FeedStatus::Running);

        let tab_elements: Vec<_> = tabs
            .map(|tabs| {
                tabs.tabs
                    .iter()
                    .enumerate()
                    .map(|(ix, tab)| {
                        let selected = ix == active;
                        let key = tab.key;
                        // PTY tabs: fixed sequential label (comet: "Terminal
                        // N") — the OSC title never replaces it. The agent
                        // tab's title is computed live from the chat config.
                        let title = if tab.kind == TabKind::Agent {
                            agent_title.clone()
                        } else {
                            tab.title.clone()
                        };
                        let exited = tab.exited.is_some();
                        (ix, key, tab.kind, title, selected, exited)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let bar_chat = chat_owned.clone();
        let drop_chat = chat_owned.clone();
        // Comet terminal-panel.tsx: `flex h-10 items-center border-b
        // border-white/[0.07] pl-2 pr-1.5` on the #090909 panel — no separate
        // bar fill.
        div()
            .id("terminal-tab-bar")
            .h(px(TAB_BAR_HEIGHT))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .border_b_1()
            .border_color(crate::theme::white_alpha(0.07))
            .on_drag_move::<TabDragPayload>(cx.listener(
                move |this, event: &gpui::DragMoveEvent<TabDragPayload>, _, cx| {
                    let payload = event.drag(cx);
                    if payload.chat != bar_chat {
                        return;
                    }
                    let from = payload.from;
                    let rel_x = f32::from(event.event.position.x) - f32::from(event.bounds.left());
                    // Slot 0 is the pinned agent tab — PTYs drop at 1+.
                    let over = drop_index(rel_x, TAB_WIDTH, count).max(1);
                    this.update_drag_over(from, over, cx);
                },
            ))
            .on_drop::<TabDragPayload>(cx.listener(move |this, payload: &TabDragPayload, _, cx| {
                if payload.chat != drop_chat {
                    this.drag = None;
                    cx.notify();
                    return;
                }
                let to = this
                    .drag
                    .as_ref()
                    .map(|d| d.over)
                    .unwrap_or(payload.from)
                    .max(1);
                let chat = drop_chat.clone();
                this.commit_reorder(&chat, payload.from, to, cx);
            }))
            .children(
                tab_elements
                    .into_iter()
                    .map(|(ix, key, kind, title, selected, exited)| {
                        let chat_select = chat_owned.clone();
                        let chat_close = chat_owned.clone();
                        let chat_close2 = chat_owned.clone();
                        let chat_drag = chat_owned.clone();
                        let ghost_title = title.clone();
                        // Comet tab: `h-7 rounded-lg pl-2 pr-1 gap-1.5 text-xs`,
                        // terminal glyph + label + close; active = white/8 wash.
                        let (text_color, bg, glyph_alpha) = if selected {
                            (theme.text, crate::theme::white_alpha(0.08), 0.8)
                        } else {
                            (theme.text_muted.opacity(0.6), gpui::transparent_black(), 0.6)
                        };
                        let close_btn = div()
                            .id(("terminal-tab-close", key))
                            .size(px(20.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(6.0))
                            .when(!selected, |el| el.invisible())
                            .cursor_pointer()
                            .hover(|s| s.bg(crate::theme::white_alpha(0.09)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(&chat_close2, key, window, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE)
                                    .size(px(12.0))
                                    .text_color(theme.text_muted.opacity(0.8)),
                            );
                        // Glyph: PTYs get the terminal mark; the agent tab
                        // shows a live spinner while a command runs, else the
                        // harness brand mark.
                        let glyph: AnyElement = if kind == TabKind::Agent {
                            if agent_running {
                                div()
                                    .size(px(16.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(crate::loaders::mini_gradient_spinner(
                                        format!("agent-tab-spin-{key}"),
                                        2.0,
                                    ))
                                    .into_any_element()
                            } else {
                                let (path, tint) = crate::pickers::harness_brand_icon(agent_harness);
                                crate::icons::icon(path)
                                    .size(px(16.0))
                                    .text_color(tint.unwrap_or(text_color.opacity(glyph_alpha)))
                                    .into_any_element()
                            }
                        } else {
                            crate::icons::icon(crate::icons::TERMINAL)
                                .size(px(16.0))
                                .text_color(text_color.opacity(glyph_alpha))
                                .into_any_element()
                        };
                        let tab_el = div()
                            .id(("terminal-tab", key))
                            .w(px(TAB_WIDTH))
                            .h(px(28.0))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .pl(px(8.0))
                            .pr(px(4.0))
                            .rounded(px(8.0))
                            // comet terminal-panel.tsx tab: `transition-colors`.
                            .bg(motion::hover_blend(
                                &format!("term-tab-{key}"),
                                bg,
                                theme.element_hover,
                            ))
                            .on_hover(motion::hover_listener(format!("term-tab-{key}")))
                            .text_size(px(12.0))
                            .text_color(text_color)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_tab(&chat_select, ix, cx);
                            }))
                            // PTY tabs: middle-click closes (§1.10), drag reorders.
                            // The pinned agent tab does neither.
                            .when(kind == TabKind::Pty, |el| {
                                el.on_mouse_down(
                                    MouseButton::Middle,
                                    cx.listener(move |this, _, window, cx| {
                                        this.close_tab(&chat_close, key, window, cx);
                                    }),
                                )
                                .on_drag(
                                    TabDragPayload {
                                        chat: chat_drag,
                                        from: ix,
                                        title: ghost_title,
                                    },
                                    |payload, _point, _, cx| {
                                        let title = payload.title.clone();
                                        cx.stop_propagation();
                                        cx.new(|_| TabGhost { title })
                                    },
                                )
                            })
                            .when(exited, |el| el.opacity(0.55))
                            .child(glyph)
                            .child(div().flex_1().min_w_0().truncate().child(title))
                            .when(kind == TabKind::Pty, |el| el.child(close_btn));

                        // Sliding transform while a sibling is dragged over: animate
                        // 150 ms between committed offsets.
                        match drag {
                            Some((from, over, epoch, prev_over)) if ix != from => {
                                let target = slide_offset(ix, from, over) * TAB_WIDTH;
                                let start = slide_offset(ix, from, prev_over) * TAB_WIDTH;
                                div()
                                    .relative()
                                    .child(tab_el.with_animation(
                                        ("terminal-tab-slide", key | ((epoch as u64) << 32)),
                                        TAB_SLIDE.animation(),
                                        move |el, t| el.left(px(motion::lerp(start, target, t))),
                                    ))
                                    .into_any_element()
                            }
                            // Invisible spacer — the ghost carries the tab; a
                            // dimmed original overlapped the sibling that
                            // slides into the vacated slot.
                            Some((from, ..)) if ix == from => div()
                                .w(px(TAB_WIDTH))
                                .h(px(28.0))
                                .flex_none()
                                .into_any_element(),
                            _ => tab_el.into_any_element(),
                        }
                    }),
            )
            .child(
                div()
                    .id("terminal-new-tab")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    // comet terminal-panel.tsx icon buttons: `transition-colors`.
                    .bg(motion::hover_blend(
                        "term-new-tab",
                        gpui::transparent_black(),
                        crate::theme::white_alpha(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-new-tab"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(chat) = this.selected_chat(cx) {
                            this.open_tab(chat, cx);
                        }
                    }))
                    .child(
                        crate::icons::icon(crate::icons::PLUS)
                            .size(px(16.0))
                            .text_color(theme.text_muted.opacity(0.6)),
                    ),
            )
            // Collapse chevron pinned right (comet "Hide terminal" ⌘J).
            .child(div().flex_1())
            .child(
                div()
                    .id("terminal-collapse")
                    .size(px(28.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "term-collapse",
                        gpui::transparent_black(),
                        crate::theme::white_alpha(0.05),
                    ))
                    .on_hover(motion::hover_listener("term-collapse"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleTerminal), cx);
                    })
                    .child(
                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                            .size(px(13.0))
                            .text_color(theme.text_muted.opacity(0.55)),
                    ),
            )
    }
}

enum StreamDisposition {
    Continue,
    Stop,
}

/// One feed row: a command line in the agent's terminal voice — a status
/// glyph (spinner while running, `✗` on failure, `$` otherwise) and the
/// single-lined command, with a chevron marking it expandable. Deep-linked
/// rows flash a wash. Expanded rows gain an output block under the command
/// (variable height — the deep-link anchor still lands, gpui measures).
fn feed_row(entry: &FeedEntry, flashing: bool, expansion: &FeedExpansion, theme: &Theme) -> gpui::Div {
    let glyph: AnyElement = match entry.status {
        FeedStatus::Running => div()
            .size(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(crate::loaders::mini_gradient_spinner(
                format!("feed-spin-{}", entry.id),
                2.0,
            ))
            .into_any_element(),
        FeedStatus::Failed => div()
            .size(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.0))
            .text_color(theme.danger)
            .child(SharedString::from("✗"))
            .into_any_element(),
        // Finished rows — resolved or dead — read as settled history. A
        // command whose session died mid-run never spins forever.
        FeedStatus::Ok | FeedStatus::Unfinished => div()
            .size(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.0))
            .text_color(theme.text_faint)
            .child(SharedString::from("$"))
            .into_any_element(),
    };
    let text_color = match entry.status {
        FeedStatus::Running | FeedStatus::Failed => theme.text,
        FeedStatus::Ok | FeedStatus::Unfinished => theme.text_muted,
    };
    let expanded = !matches!(expansion, FeedExpansion::Collapsed);
    let command_line = div()
        .h(px(FEED_ROW_HEIGHT))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .rounded(px(4.0))
        .when(flashing, |el| el.bg(crate::theme::white_alpha(0.06)))
        .child(glyph)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(theme.font_mono.clone())
                .text_size(px(12.0))
                .text_color(text_color)
                .child(SharedString::from(entry.command.clone())),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(10.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(if expanded { "▾" } else { "▸" })),
        );
    let mut row = div().flex_none().flex().flex_col().child(command_line);
    // Output block, aligned under the command text (past glyph + gap).
    let note = |text: &str| {
        div()
            .flex_none()
            .pl(px(24.0))
            .pb(px(4.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(11.0))
            .text_color(theme.text_faint)
            .child(SharedString::from(text.to_owned()))
    };
    match expansion {
        FeedExpansion::Collapsed => {}
        FeedExpansion::StillRunning => row = row.child(note("still running…")),
        FeedExpansion::Loading => row = row.child(note("fetching output…")),
        FeedExpansion::Unavailable => row = row.child(note("output unavailable")),
        FeedExpansion::NoOutput => row = row.child(note("(no output)")),
        FeedExpansion::Lines(lines) => {
            row = row.child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .pl(px(24.0))
                    .pb(px(4.0))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .children(lines.iter().map(|line| {
                        div()
                            .flex_none()
                            .truncate()
                            .child(SharedString::from(line.clone()))
                    })),
            );
        }
    }
    row
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // Heal drag state if the pointer was released outside the bar.
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
        let Some(chat) = self.selected_chat(cx) else {
            return div()
                .size_full()
                .bg(terminal_bg())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("Select a chat to open a terminal"))
                .into_any_element();
        };
        let focused = self.focus_handle.is_focused(window);
        let agent_active = self.active_is_agent(cx);

        // The agent feed is read-only: no PTY key encoding, no emulator
        // scroll — its own overflow scroll handles the wheel.
        let body: AnyElement = if agent_active {
            div()
                .id("terminal-body")
                .flex_1()
                .min_h_0()
                .track_focus(&self.focus_handle)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window: &mut Window, cx| {
                        window.focus(&this.focus_handle, cx);
                    }),
                )
                .child(self.render_agent_feed(&chat, &theme, cx))
                .into_any_element()
        } else {
            div()
                .id("terminal-body")
                .flex_1()
                .min_h_0()
                .key_context("Terminal")
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::on_key_down))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window: &mut Window, cx| {
                        window.focus(&this.focus_handle, cx);
                    }),
                )
                .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                    let lines = match event.delta {
                        ScrollDelta::Lines(delta) => delta.y,
                        ScrollDelta::Pixels(delta) => {
                            f32::from(delta.y) / super::view::TERM_LINE_HEIGHT
                        }
                    };
                    let step = lines.round() as i32;
                    this.scroll_active(step, cx);
                }))
                .child(TerminalElement::new(cx.entity(), focused))
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(terminal_bg())
            .child(self.render_tab_bar(&chat, cx))
            .child(body)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_display_lines_tail_caps_with_markers() {
        // Short output passes through untouched.
        assert_eq!(output_display_lines("a\nb", false), vec!["a", "b"]);
        // View cap: last FEED_OUTPUT_MAX_LINES kept, hidden count marked.
        let big: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let lines = output_display_lines(&big, false);
        assert_eq!(lines.len(), FEED_OUTPUT_MAX_LINES + 1);
        assert_eq!(lines[0], "… 6 earlier lines");
        assert_eq!(lines[1], "line 7");
        assert_eq!(lines.last().unwrap(), "line 30");
        // Capture truncation gets its own marker, both markers stack.
        let lines = output_display_lines(&big, true);
        assert_eq!(lines[0], "… truncated to the last 64 KB");
        assert_eq!(lines[1], "… 6 earlier lines");
    }

    fn exec_entry(
        id: &str,
        command: &str,
        is_error: bool,
        resolved: bool,
    ) -> comet_doc::SessionMessageEntry {
        comet_doc::SessionMessageEntry {
            id: id.into(),
            role: comet_doc::MessageRole::Assistant,
            parts: vec![MessagePart::Tool {
                id: format!("tool-{id}"),
                call: ToolCall::Exec {
                    command: command.into(),
                },
                is_error,
                resolved,
            }],
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    #[test]
    fn exec_feed_extracts_commands_in_transcript_order() {
        let mut text_entry = exec_entry("m0", "ignored", false, true);
        text_entry.parts = vec![MessagePart::Text {
            id: "t".into(),
            text: "prose".into(),
        }];
        let entries = vec![
            text_entry,
            exec_entry("m1", "ls -la", false, true),
            exec_entry("m2", "cargo test\n--quiet", false, false),
            exec_entry("m3", "false", true, true),
        ];
        let feed = exec_feed(&entries, true);
        assert_eq!(feed.len(), 3, "non-exec parts never enter the feed");
        assert_eq!(feed[0].command, "ls -la");
        assert_eq!(feed[0].status, FeedStatus::Ok);
        // Multi-line commands collapse to one terminal line.
        assert_eq!(feed[1].command, "cargo test --quiet");
        assert_eq!(feed[1].status, FeedStatus::Running);
        assert_eq!(feed[2].status, FeedStatus::Failed);
        // Deep-link ids are the tool part ids.
        assert_eq!(feed[1].id, "tool-m2");
    }

    #[test]
    fn exec_feed_unresolved_in_a_dead_session_is_not_running() {
        let entries = vec![exec_entry("m1", "sleep 99", false, false)];
        assert_eq!(exec_feed(&entries, true)[0].status, FeedStatus::Running);
        // A crashed session must never leave an eternal spinner.
        assert_eq!(
            exec_feed(&entries, false)[0].status,
            FeedStatus::Unfinished
        );
    }

    #[test]
    fn follow_target_anchor_wins_then_fingerprint_changes_follow_tail() {
        // Deep-link anchor wins outright.
        assert_eq!(follow_target((5, 0), (5, 0), Some(2)), Some(2));
        assert_eq!(follow_target((5, 0), (6, 1), Some(0)), Some(0));
        // A grown feed pins to its new tail.
        assert_eq!(follow_target((5, 3), (6, 1), None), Some(5));
        assert_eq!(follow_target((0, 0), (4, 2), None), Some(3));
        // Same length but the tail row's expansion changed (output loaded,
        // auto-collapse as the tail moved): still follows.
        assert_eq!(follow_target((5, 1), (5, 24), None), Some(4));
        assert_eq!(follow_target((5, 24), (5, 0), None), Some(4));
        // A fully unchanged fingerprint never scrolls.
        assert_eq!(follow_target((5, 3), (5, 3), None), None);
        assert_eq!(follow_target((0, 0), (0, 0), None), None);
    }

    #[test]
    fn feed_follow_gates_on_at_bottom() {
        // Pinned to the tail (offset == -max): follow engages.
        assert!(feed_at_bottom(-480.0, 480.0));
        // Sub-pixel landing tolerance still counts.
        assert!(feed_at_bottom(-478.5, 480.0));
        // Scrolled back even a line: follow disengages.
        assert!(!feed_at_bottom(-460.0, 480.0));
        // Fresh handle / content that fits (max 0, offset 0): follow engages
        // so a chat switch's first frame and short feeds still tail.
        assert!(feed_at_bottom(0.0, 0.0));
    }

    #[test]
    fn feed_row_expansion_auto_tail_with_manual_overrides() {
        let pinned =
            |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<HashSet<String>>();
        // The latest row auto-expands; older rows auto-collapse.
        assert!(is_feed_row_expanded(None, None, "c", true));
        assert!(!is_feed_row_expanded(None, None, "a", false));
        // A pinned older row stays expanded.
        assert!(is_feed_row_expanded(Some(&pinned(&["a"])), None, "a", false));
        // A dismissed latest row stays collapsed.
        assert!(!is_feed_row_expanded(None, Some(&pinned(&["c"])), "c", true));
        // A dismissal on an older row is a no-op; a pin on the latest is a no-op.
        assert!(!is_feed_row_expanded(None, Some(&pinned(&["a"])), "a", false));
        assert!(is_feed_row_expanded(Some(&pinned(&["c"])), None, "c", true));
    }

    #[test]
    fn agent_tab_name_rules() {
        use comet_proto::{ChatConfig, ReasoningLevel, SandboxLevel};
        let config = |harness, model: Option<&str>| ChatConfig {
            harness,
            model: model.map(str::to_string),
            reasoning: None::<ReasoningLevel>,
            model_options: serde_json::Map::new(),
            sandbox: SandboxLevel::WorkspaceWrite,
        };
        // Model wins, provider prefix stripped.
        assert_eq!(
            agent_tab_name(Some(&config(HarnessId::Pi, Some("anthropic/claude-opus-4.5")))),
            "claude-opus-4.5"
        );
        assert_eq!(
            agent_tab_name(Some(&config(HarnessId::Pi, Some("gpt-5.1")))),
            "gpt-5.1"
        );
        // No model → harness brand name.
        assert_eq!(agent_tab_name(Some(&config(HarnessId::Pi, None))), "pi");
        // No config at all → generic.
        assert_eq!(agent_tab_name(None), "agent");
        // The title wraps the name with the possessive.
        assert_eq!(agent_tab_title(None).as_ref(), "agent's terminal");
    }

    #[test]
    fn short_model_name_strips_provider_prefix() {
        assert_eq!(short_model_name("anthropic/claude-opus-4.5"), "claude-opus-4.5");
        assert_eq!(short_model_name("gpt-5.1"), "gpt-5.1");
        assert_eq!(short_model_name(""), "");
    }

    #[test]
    fn pty_numbering_skips_the_pinned_agent_tab() {
        assert_eq!(next_pty_number([TabKind::Agent].into_iter()), 1);
        assert_eq!(next_pty_number([TabKind::Agent, TabKind::Pty].into_iter()), 2);
        assert_eq!(next_pty_number([].into_iter()), 1);
    }

    #[test]
    fn height_clamps_between_160_and_55vh() {
        assert_eq!(clamp_terminal_height(300.0, 900.0), 300.0);
        assert_eq!(clamp_terminal_height(10.0, 900.0), 160.0);
        assert_eq!(clamp_terminal_height(4000.0, 900.0), 900.0 * 0.55);
        // Tiny windows: min wins over the 55vh cap.
        assert_eq!(clamp_terminal_height(200.0, 100.0), 160.0);
        assert_eq!(clamp_terminal_height(f32::NAN, 900.0), 160.0);
    }

    #[test]
    fn drop_index_clamped_keeps_agent_tab_pinned() {
        // Slot 0 belongs to the agent tab: every drop position clamps to 1+.
        // (drop_index is floor-based: hovering slot k lands at index k.)
        assert_eq!(drop_index(0.0, TAB_WIDTH, 4).max(1), 1);
        assert_eq!(drop_index(TAB_WIDTH * 0.5, TAB_WIDTH, 4).max(1), 1);
        assert_eq!(drop_index(TAB_WIDTH * 1.5, TAB_WIDTH, 4).max(1), 1);
        assert_eq!(drop_index(TAB_WIDTH * 2.5, TAB_WIDTH, 4).max(1), 2);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_ms(0), 500);
        assert_eq!(backoff_ms(1), 1000);
        assert_eq!(backoff_ms(2), 2000);
        assert_eq!(backoff_ms(3), 4000);
        assert_eq!(backoff_ms(4), 8000);
        assert_eq!(backoff_ms(10), 8000);
        assert_eq!(backoff_ms(u32::MAX), 8000);
    }

    #[test]
    fn reorder_moves_forward_and_backward() {
        let mut v = vec!["a", "b", "c", "d"];
        reorder_tabs(&mut v, 0, 2);
        assert_eq!(v, ["b", "c", "a", "d"]);
        reorder_tabs(&mut v, 3, 0);
        assert_eq!(v, ["d", "b", "c", "a"]);
        // Out-of-range / no-op moves leave the vec untouched.
        reorder_tabs(&mut v, 9, 0);
        reorder_tabs(&mut v, 1, 1);
        assert_eq!(v, ["d", "b", "c", "a"]);
    }

    #[test]
    fn drop_index_quantizes_and_clamps() {
        assert_eq!(drop_index(-10.0, 150.0, 3), 0);
        assert_eq!(drop_index(0.0, 150.0, 3), 0);
        assert_eq!(drop_index(149.0, 150.0, 3), 0);
        assert_eq!(drop_index(150.0, 150.0, 3), 1);
        assert_eq!(drop_index(700.0, 150.0, 3), 2);
        assert_eq!(drop_index(50.0, 150.0, 0), 0);
    }

    #[test]
    fn slide_offsets_shift_toward_the_gap() {
        // Dragging 0 over 2: tabs 1 and 2 slide left one slot.
        assert_eq!(slide_offset(0, 0, 2), 0.0);
        assert_eq!(slide_offset(1, 0, 2), -1.0);
        assert_eq!(slide_offset(2, 0, 2), -1.0);
        assert_eq!(slide_offset(3, 0, 2), 0.0);
        // Dragging 3 over 1: tabs 1 and 2 slide right.
        assert_eq!(slide_offset(0, 3, 1), 0.0);
        assert_eq!(slide_offset(1, 3, 1), 1.0);
        assert_eq!(slide_offset(2, 3, 1), 1.0);
        assert_eq!(slide_offset(3, 3, 1), 0.0);
        // Hovering the origin: nothing moves.
        for ix in 0..4 {
            assert_eq!(slide_offset(ix, 2, 2), 0.0);
        }
    }

    #[test]
    fn active_index_tracks_reorders() {
        // The active tab itself moves.
        assert_eq!(active_after_reorder(1, 1, 3), 3);
        // A tab hopping over the active one from the left shifts it down.
        assert_eq!(active_after_reorder(2, 0, 3), 1);
        // …and from the right shifts it up.
        assert_eq!(active_after_reorder(1, 3, 0), 2);
        // Disjoint moves leave it alone.
        assert_eq!(active_after_reorder(0, 2, 3), 0);
    }

    #[test]
    fn active_index_tracks_closes() {
        assert_eq!(active_after_close(2, 0, 3), 1); // close left of active
        assert_eq!(active_after_close(1, 1, 2), 1); // close active mid-list
        assert_eq!(active_after_close(2, 2, 2), 1); // close active at tail
        assert_eq!(active_after_close(0, 0, 0), 0); // last tab closed
    }

    #[test]
    fn exit_message_format() {
        let text = String::from_utf8(exit_message(0)).unwrap();
        assert!(text.contains("[process exited 0]"));
        let text = String::from_utf8(exit_message(137)).unwrap();
        assert!(text.contains("[process exited 137]"));
        assert!(text.starts_with("\r\n"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn shell_titles() {
        assert_eq!(shell_title("/bin/zsh"), "zsh");
        assert_eq!(shell_title("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_title("C:\\Windows\\System32\\cmd.exe"), "cmd.exe");
        assert_eq!(shell_title("bash"), "bash");
        assert_eq!(shell_title(""), "terminal");
    }

    #[test]
    fn stream_events_deserialize_per_contract() {
        let data: TerminalEvent =
            serde_json::from_str(r#"{"type":"data","seq":7,"data":"aGk="}"#).unwrap();
        assert_eq!(
            data,
            TerminalEvent::Data {
                seq: 7,
                data: "aGk=".into()
            }
        );
        let exit: TerminalEvent =
            serde_json::from_str(r#"{"type":"exit","seq":8,"exitCode":130}"#).unwrap();
        assert_eq!(
            exit,
            TerminalEvent::Exit {
                seq: 8,
                exit_code: 130,
                signal: None
            }
        );
        let session: TerminalSession =
            serde_json::from_str(r#"{"id":"t1","cwd":"/w","shell":"/bin/zsh"}"#).unwrap();
        assert_eq!(session.id, "t1");
        assert_eq!(session.shell, "/bin/zsh");
    }

    #[test]
    fn base64_round_trip_and_tolerance() {
        assert_eq!(decode_base64("aGk="), b"hi".to_vec());
        assert_eq!(
            decode_base64("aGk"),
            b"hi".to_vec(),
            "unpadded input tolerated"
        );
        assert_eq!(
            decode_base64("!!!"),
            Vec::<u8>::new(),
            "garbage decodes to nothing"
        );
        assert_eq!(encode_base64(b"hi"), "aGk=");
    }

    #[test]
    fn exit_message_feeds_cleanly_through_the_emulator() {
        let mut emulator = Emulator::new(40, 4);
        emulator.feed(b"$ done");
        emulator.feed(&exit_message(1));
        assert_eq!(emulator.row_text(1), "[process exited 1]");
    }
}
