//! Frontend-agnostic view logic: the derivations every viewport needs and none
//! of them should own — sort orders, staleness gating, sidebar grouping, the
//! boot gate, relative times.
//!
//! This lives in `proto` rather than in a viewport crate because comet-native
//! has two of them (the gpui app in `comet-ui`, the terminal app in
//! `comet-tui`) and a *divergent* sort order between them is a real bug: the
//! same workspace doc must produce the same row order on every surface. Both
//! crates re-export from here, so there is exactly one implementation and one
//! test suite per rule.
//!
//! Everything in this module is pure. `chat_indicator` (the status derivation
//! these gate on) is in [`crate::entities`].

use chrono::{DateTime, Utc};

use crate::{AuthState, Chat, ChatIndicator, ChatUsage, Project, Session, SessionStatus};

// ---------------------------------------------------------------------------
// Connection + status
// ---------------------------------------------------------------------------

/// Viewport ⇄ engine connection lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Ready,
    Failed(String),
}

/// What a chat's status dot / working indicator should show right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    None,
    Working,
    AwaitingInput,
    Errored,
}

/// A `Working`/`AwaitingInput` session older than this is treated as dead — a
/// crashed backend must never show an eternal "Working" (feature-inventory
/// §1.12). Engines heartbeat sessions well inside this window.
pub const SESSION_STALE_MS: i64 = 45_000;

/// Staleness-checked indicator for a session row. Pure.
pub fn effective_indicator(session: Option<&Session>, now: DateTime<Utc>) -> Indicator {
    let Some(session) = session else {
        return Indicator::None;
    };
    match session.status {
        SessionStatus::Idle => Indicator::None,
        SessionStatus::Errored => Indicator::Errored,
        SessionStatus::Working | SessionStatus::AwaitingInput => {
            let age_ms = now
                .signed_duration_since(session.updated_at)
                .num_milliseconds();
            if age_ms > SESSION_STALE_MS {
                Indicator::None
            } else if session.status == SessionStatus::Working {
                Indicator::Working
            } else {
                Indicator::AwaitingInput
            }
        }
    }
}

/// The full display status for a chat row / tab dot: live states win, then the
/// synced seen marker decides completed-vs-idle. Staleness gating rides on
/// [`effective_indicator`]; the derivation itself is [`crate::chat_indicator`].
pub fn display_status(chat: &Chat, session: Option<&Session>, now: DateTime<Utc>) -> ChatIndicator {
    let live = session.filter(|s| effective_indicator(Some(s), now) != Indicator::None);
    crate::chat_indicator(chat, live)
}

/// Attention bucket for the sidebar's Active list — lower is more urgent.
pub fn attention_rank(status: ChatIndicator) -> u8 {
    match status {
        ChatIndicator::AwaitingInput => 0,
        ChatIndicator::Errored => 1,
        ChatIndicator::Working => 2,
        ChatIndicator::Completed => 3,
        ChatIndicator::Idle => 4,
    }
}

// ---------------------------------------------------------------------------
// Sort orders
// ---------------------------------------------------------------------------

/// Active-list order: pure recency (`last_message_at` desc, `created_at`
/// fallback), id tiebreak so the sort is total. Deliberately NOT
/// attention-bucketed: status drives the DOT, never the position — bucketing
/// meant that merely OPENING a completed session (completed → seen → idle)
/// dropped its row under the pointer (user report: "their position in the
/// scrollbar changes"). Matches the old sidebar, which rendered chats in
/// recency order and let the dots carry urgency; [`attention_rank`] still
/// aggregates the project rows' urgency dot.
pub fn sort_active(rows: &mut Vec<(ChatIndicator, &Chat)>) {
    rows.sort_by(|(_, a), (_, b)| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
}

/// Session-tab order for a project: creation order (activity never reorders
/// tabs), id tiebreak. Pure.
pub fn sort_tabs(chats: &mut [&Chat]) {
    chats.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Projects list order: creation order, id tiebreak — total and stable across
/// devices. Pure.
pub fn sort_projects(projects: &mut [Project]) {
    projects.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Sidebar order: `last_message_at` desc, falling back to `created_at`; ties
/// break by `created_at` desc then id so the sort is total and stable across
/// devices. Pure.
pub fn sort_chats(chats: &mut [Chat]) {
    chats.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

// ---------------------------------------------------------------------------
// Boot gate
// ---------------------------------------------------------------------------

/// The app gate (comet's App.tsx phases). Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatePhase {
    /// Booting / probing — splash covers this.
    Loading,
    /// Engine unreachable and embedding failed.
    Failed(String),
    /// Engine up, but signed out — show the sign-in card.
    SignIn,
    /// Signed in but no organization selected — "Create your workspace".
    OrgGate,
    /// Render the shell.
    Ready,
}

/// `auth = None` means "engine doesn't report auth yet" (dev mode) and gates
/// nothing.
pub fn gate_phase(connection: &ConnectionStatus, auth: Option<&AuthState>) -> GatePhase {
    match connection {
        ConnectionStatus::Connecting => GatePhase::Loading,
        ConnectionStatus::Failed(err) => GatePhase::Failed(err.clone()),
        ConnectionStatus::Ready => match auth {
            Some(AuthState::SignedOut) => GatePhase::SignIn,
            Some(AuthState::NeedsOrganization { .. }) => GatePhase::OrgGate,
            _ => GatePhase::Ready,
        },
    }
}

/// Parse an `AuthStatus` frame tolerantly. The engine currently serializes its
/// own enum (`{"_tag": "SignedIn", ...}`) while the proto type expects
/// `{"state": "signedIn", ...}` — accept both so either side can converge
/// without breaking a viewport.
pub fn parse_auth_state(value: &serde_json::Value) -> Option<AuthState> {
    if let Ok(state) = serde_json::from_value::<AuthState>(value.clone()) {
        return Some(state);
    }
    let tag = value.get("_tag").and_then(|t| t.as_str())?;
    let user = || -> Option<crate::UserProfile> {
        let u = value.get("user")?;
        Some(crate::UserProfile {
            id: u.get("id")?.as_str()?.to_string(),
            email: u.get("email")?.as_str()?.to_string(),
            name: u.get("name").and_then(|n| n.as_str()).map(str::to_string),
        })
    };
    match tag {
        "SignedOut" => Some(AuthState::SignedOut),
        "NeedsOrganization" => Some(AuthState::NeedsOrganization { user: user()? }),
        "SignedIn" => Some(AuthState::SignedIn {
            user: user()?,
            org_id: value
                .get("orgId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sidebar grouping
// ---------------------------------------------------------------------------

/// One grouped-by-project sidebar section.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatGroup<'a> {
    pub label: String,
    pub chats: Vec<&'a Chat>,
}

/// Project label for a chat: the basename of its cwd, or "No project".
pub fn project_label(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) else {
        return "No project".to_string();
    };
    std::path::Path::new(cwd.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

/// Group chats by project label, preserving the incoming (recency) order both
/// for groups (by their most recent chat) and rows within a group. Pure.
pub fn group_chats<'a>(chats: impl IntoIterator<Item = &'a Chat>) -> Vec<ChatGroup<'a>> {
    let mut groups: Vec<ChatGroup<'a>> = Vec::new();
    for chat in chats {
        let label = project_label(chat.cwd.as_deref());
        match groups.iter_mut().find(|g| g.label == label) {
            Some(group) => group.chats.push(chat),
            None => groups.push(ChatGroup {
                label,
                chats: vec![chat],
            }),
        }
    }
    groups
}

/// Compact relative time ("now", "5m", "3h", "2d", "1w", …) — no "ago" suffix;
/// port of comet's `formatTimeAgo`.
pub fn format_time_ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = now.signed_duration_since(then).num_seconds().max(0);
    // Under a minute reads as "now" — otherwise 45–59s floors to a bare "0m".
    if s < 60 {
        return "now".to_string();
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    if w < 5 {
        return format!("{w}w");
    }
    let mo = d / 30;
    if mo < 12 {
        return format!("{mo}mo");
    }
    format!("{}y", d / 365)
}

// ---------------------------------------------------------------------------
// Run cost — tokens/sec and the context gauge (both viewports render these)
// ---------------------------------------------------------------------------

/// Generation speed of the last reply. `None` when either half is missing, so
/// a surface never renders a rate it did not measure.
pub fn tokens_per_sec(usage: &ChatUsage) -> Option<f32> {
    (usage.last_turn_tokens > 0 && usage.last_turn_ms > 0)
        .then(|| usage.last_turn_tokens as f32 * 1000.0 / usage.last_turn_ms as f32)
}

/// "47 tok/s" — one decimal under 10, where the difference is worth reading.
pub fn format_rate(rate: f32) -> String {
    format!("{} tok/s", format_rate_value(rate))
}

/// Just the number, for surfaces that set the unit in its own type (the gpui
/// strip runs the digits in mono and the unit in the text face — one string
/// would carry the mono space between them, and it reads as a gap).
pub fn format_rate_value(rate: f32) -> String {
    if rate < 10.0 {
        format!("{rate:.1}")
    } else {
        format!("{rate:.0}")
    }
}

/// How full the model's context is. The gauge is a battery: it starts full and
/// green and drains toward red as the conversation fills the window, so
/// [`ContextGauge::level`] is how many cells are still LIT.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextGauge {
    /// Fraction of the window in use, clamped to 0..=1.
    pub fraction: f32,
    /// Cells lit, 1..=5 — the discrete state the color steps through. Never
    /// zero: an empty battery would read as "no reading", and when there is
    /// no reading there is no gauge at all.
    pub level: u8,
}

/// The gauge for a stamped usage, or `None` when the window is unknown (no
/// denominator, no reading) or nothing has been counted yet.
pub fn context_gauge(usage: &ChatUsage) -> Option<ContextGauge> {
    if usage.context_window == 0 || usage.context_tokens == 0 {
        return None;
    }
    let fraction = (usage.context_tokens as f32 / usage.context_window as f32).clamp(0.0, 1.0);
    // Thresholds sit where the reading changes meaning, not on even fifths:
    // the last cell is a warning — pi auto-compacts in the high 80s — so it
    // gets the narrow band, and the roomy half of the window gets the wide one.
    let level = match fraction {
        f if f < 0.25 => 5,
        f if f < 0.50 => 4,
        f if f < 0.70 => 3,
        f if f < 0.85 => 2,
        _ => 1,
    };
    Some(ContextGauge { fraction, level })
}

/// "62k / 200k · 31%" — the gauge's own words, for a tooltip or a status line.
pub fn format_context(usage: &ChatUsage) -> String {
    let pct = context_gauge(usage)
        .map(|g| g.fraction * 100.0)
        .unwrap_or(0.0);
    format!(
        "{} / {} · {pct:.0}%",
        compact_count(usage.context_tokens),
        compact_count(usage.context_window)
    )
}

/// Token counts read as magnitudes, not digits: 62k, 1.2M.
pub fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f32 / 1_000_000.0),
    }
}

/// Session-row sub-line, "project · branch" (comet `chatLocation`): the repo
/// checkout identity. Either part may be missing; empty when both are.
pub fn chat_location(chat: &Chat) -> Option<String> {
    let project = chat
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| project_label(Some(c)));
    let reference = chat
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    match (project, reference) {
        (Some(p), Some(r)) => Some(format!("{p} · {r}")),
        (Some(p), None) => Some(p),
        (None, Some(r)) => Some(r.to_string()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries (pure)
// ---------------------------------------------------------------------------

/// Collapse model-generated text onto ONE line for single-line surfaces (tool
/// chips, titles, previews): newlines, tabs and runs of whitespace become
/// single projects, trimmed.
///
/// Both viewports need this for the same reason from opposite directions — gpui
/// breaks on a literal `\n` before its ellipsis logic, and a terminal cell grid
/// would take an embedded newline as a cursor move.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Leading program names of a shell command string, in segment order WITH
/// duplicates (multiplicity is the caller's signal — eight `git` segments
/// should not read as one). For summarizing exec runs: "ran git, cargo".
///
/// Models chain commands in one shell call (`git log --oneline && git status`,
/// multi-line scripts), so the string is split into segments on `&&`, `||`,
/// `;`, `|`, and newlines OUTSIDE quotes (`grep "a; b"` stays one segment),
/// then each segment contributes its first real word: `VAR=val` prefixes,
/// wrappers (`sudo`, `env`, …), flags, and pure builtins (`cd`, `export`,
/// `set`) are skipped, and paths are basenamed (`/usr/bin/git` → `git`).
///
/// This is a heuristic, not a shell grammar — subshells and wrapper args are
/// not modeled. The worst case is an odd name on a summary line, never a
/// behavior change; an unparseable command yields an empty list and the
/// caller falls back to a bare count.
pub fn command_names(command: &str) -> Vec<String> {
    split_shell_segments(command)
        .iter()
        .flat_map(|segment| segment_names(segment, false))
        .collect()
}

/// Split on unquoted `&`, `|`, `;`, `\n`. Single quotes are literal, double
/// quotes allow backslash escapes, backslash escapes the next byte outside
/// single quotes. Separator and slice indices only ever land on ASCII bytes,
/// so UTF-8 content passes through untouched.
fn split_shell_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::with_capacity(4);
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !in_single => i += 1, // skip the escaped byte too
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'&' | b'|' | b';' | b'\n' if !in_single && !in_double => {
                segments.push(&command[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    segments.push(&command[start..]);
    segments
}

/// The program names of one shell segment: its first real word, basenamed.
/// Wrappers (`sudo foo` → `foo`) are stepped through; a shell wrapper (`sh
/// -c "…"`, `bash script.sh`) contributes the names of its PAYLOAD, parsed as
/// its own command string one level deep — agents wrap chains that way
/// constantly, and naming the shell hides the real work. Segments that are
/// only assignments, flags, or a builtin contribute nothing.
fn segment_names(segment: &str, nested: bool) -> Vec<String> {
    /// Commands that execute another command — skip them to name the payload.
    const WRAPPERS: [&str; 6] = ["sudo", "env", "command", "time", "nice", "nohup"];
    /// Shells invoked with a script (`-c "…"` or a file path).
    const SHELLS: [&str; 6] = ["bash", "sh", "zsh", "nu", "dash", "fish"];
    /// Shell builtins that read as noise in a summary (and never name a
    /// payload worth showing).
    const BUILTINS: [&str; 6] = ["cd", "export", "set", "source", ".", "umask"];
    let mut words = segment.split_whitespace();
    while let Some(word) = words.next() {
        if is_assignment(word) {
            continue;
        }
        let bare = word
            .rsplit('/')
            .next()
            .unwrap_or(word)
            .trim_matches(|c| c == '"' || c == '\'');
        if bare.is_empty() {
            continue;
        }
        if BUILTINS.contains(&bare) {
            return Vec::new();
        }
        if WRAPPERS.contains(&bare) || bare.starts_with('-') {
            continue;
        }
        if !nested && SHELLS.contains(&bare) {
            // Words still carry their original quotes, so re-joining
            // reconstructs the payload's quoting exactly — peel a matched
            // pair of surrounding quotes (`-c "…"`) or the inner chain's
            // separators would stay protected.
            let payload = words
                .filter(|w| !w.starts_with('-'))
                .collect::<Vec<_>>()
                .join(" ");
            let payload = payload.trim();
            let payload = if payload.len() >= 2
                && ((payload.starts_with('"') && payload.ends_with('"'))
                    || (payload.starts_with('\'') && payload.ends_with('\'')))
            {
                &payload[1..payload.len() - 1]
            } else {
                payload
            };
            return split_shell_segments(payload)
                .iter()
                .flat_map(|s| segment_names(s, true))
                .collect();
        }
        return vec![bare.to_string()];
    }
    Vec::new()
}

/// A leading `VAR=val` shell assignment word.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Per-kind chip label + one-line detail. Labels match comet's `describeTool`
/// (tool-chip.tsx) exactly, so the two viewports name a tool identically.
pub fn tool_chip_content(call: &crate::ToolCall) -> (&'static str, String) {
    let (label, detail) = tool_chip_content_raw(call);
    (label, single_line(&detail))
}

fn tool_chip_content_raw(call: &crate::ToolCall) -> (&'static str, String) {
    use crate::ToolCall;
    match call {
        // "Exec", not "Run": every tool line reads `Label detail`, and the
        // shell must sit in that column like any other kind.
        ToolCall::Exec { command } => ("Exec", command.clone()),
        ToolCall::ReadFile { path } => ("Read", path.clone()),
        ToolCall::WriteFile { path, .. } => ("Write", path.clone()),
        ToolCall::EditFile { path, .. } => ("Edit", path.clone()),
        ToolCall::ApplyPatch { path } => {
            ("Patch", path.clone().unwrap_or_else(|| "workspace".into()))
        }
        ToolCall::Search { pattern, path } => (
            "Search",
            match path {
                Some(path) => format!("{pattern} in {path}"),
                None => pattern.clone(),
            },
        ),
        ToolCall::Glob { pattern } => ("Glob", pattern.clone()),
        ToolCall::WebFetch { url, .. } => ("Fetch", url.clone()),
        ToolCall::WebSearch { query } => ("Web", query.clone()),
        ToolCall::Todo { items } => {
            let done = items.iter().filter(|i| i.done).count();
            ("Todo", format!("{done}/{} done", items.len()))
        }
        ToolCall::Mcp { server, tool, .. } => ("MCP", format!("{server} · {tool}")),
        ToolCall::Unknown { name, .. } => ("Tool", name.clone()),
    }
}

/// Collapse a run's details into one line, folding paths that share a parent
/// into brace groups: `["a/b/x.rs", "a/b/y.rs", "t/z"]` reads
/// `a/b/{x.rs,y.rs}, t/z`. Shell-brace notation because it is the one
/// shorthand every reader of this app already parses at a glance, and it
/// keeps a long list to one honest line instead of an ellipsis.
///
/// Groups keep first-appearance order, exact repeats collapse, and anything
/// without a parent directory (patterns, queries, bare names) passes through
/// untouched — a brace group of one is never worth the punctuation.
pub fn coalesce_paths(details: &[String]) -> String {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for detail in details {
        let (dir, base) = match detail.rsplit_once('/') {
            Some((dir, base)) if !dir.is_empty() && !base.is_empty() => (dir, base),
            _ => ("", detail.as_str()),
        };
        match groups.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, bases)) => {
                if !bases.contains(&base) {
                    bases.push(base);
                }
            }
            // Groups keep insertion order — the run reads in call order.
            None => groups.push((dir, vec![base])),
        }
    }
    groups
        .iter()
        .map(|(dir, bases)| match (dir.is_empty(), bases.as_slice()) {
            (true, _) => bases.join(", "),
            (false, [only]) => format!("{dir}/{only}"),
            (false, many) => format!("{dir}/{{{}}}", many.join(",")),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// "ran cargo, git" / "ran cargo, git, npm, … (9)" / "ran 3 commands" — how a
/// run of shell commands names itself. The names come from the commands
/// themselves ([`command_names`]); the parenthesized count appears when the
/// calls outnumber the names shown, so eight `git` calls don't read as one,
/// and an unparseable set falls back to the bare count.
pub fn ran_commands(names: &[String], count: usize) -> String {
    const MAX_NAMES: usize = 3;
    if names.is_empty() {
        return format!("ran {}", plural(count, "command", "commands"));
    }
    let shown = names.len().min(MAX_NAMES);
    let mut s = format!("ran {}", names[..shown].join(", "));
    if names.len() > MAX_NAMES {
        s.push_str(", …");
    }
    if count > shown {
        s.push_str(&format!(" ({count})"));
    }
    s
}

/// The ToolGroup summary line — "Ran cargo, git · edited 2 files".
///
/// Takes `(call, is_error)` pairs so each viewport can keep its own row model;
/// the summary itself is one implementation for both.
pub fn tool_group_summary(tools: &[(crate::ToolCall, bool)]) -> String {
    use crate::ToolCall;
    let mut commands = 0usize;
    let mut cmd_names: Vec<String> = Vec::new();
    let mut edited: Vec<&str> = Vec::new();
    let mut reads = 0usize;
    let mut searches = 0usize;
    let mut fetches = 0usize;
    let mut todos = 0usize;
    let mut other = 0usize;
    let mut failed = 0usize;
    for (call, is_error) in tools {
        if *is_error {
            failed += 1;
        }
        match call {
            ToolCall::Exec { command } => {
                commands += 1;
                for name in command_names(command) {
                    if !cmd_names.contains(&name) {
                        cmd_names.push(name);
                    }
                }
            }
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
                if !edited.contains(&path.as_str()) {
                    edited.push(path);
                }
            }
            ToolCall::ApplyPatch { path } => {
                let p = path.as_deref().unwrap_or("patch");
                if !edited.contains(&p) {
                    edited.push(p);
                }
            }
            ToolCall::ReadFile { .. } => reads += 1,
            ToolCall::Search { .. } | ToolCall::Glob { .. } | ToolCall::WebSearch { .. } => {
                searches += 1
            }
            ToolCall::WebFetch { .. } => fetches += 1,
            ToolCall::Todo { .. } => todos += 1,
            ToolCall::Mcp { .. } | ToolCall::Unknown { .. } => other += 1,
        }
    }
    let mut segments: Vec<String> = Vec::new();
    if commands > 0 {
        segments.push(ran_commands(&cmd_names, commands));
    }
    if !edited.is_empty() {
        segments.push(format!("edited {}", plural(edited.len(), "file", "files")));
    }
    if reads > 0 {
        segments.push(format!("read {}", plural(reads, "file", "files")));
    }
    if searches > 0 {
        segments.push(format!("searched {}", plural(searches, "time", "times")));
    }
    if fetches > 0 {
        segments.push(format!("fetched {}", plural(fetches, "page", "pages")));
    }
    if todos > 0 {
        segments.push("updated todos".to_string());
    }
    if other > 0 {
        segments.push(format!("called {}", plural(other, "tool", "tools")));
    }
    if segments.is_empty() {
        segments.push(plural(tools.len(), "tool", "tools"));
    }
    if failed > 0 {
        segments.push(format!("{failed} failed"));
    }
    let mut summary = segments.join(" · ");
    // Capitalize the first segment only (comet's style).
    if let Some(first) = summary.get(0..1) {
        let upper = first.to_uppercase();
        summary.replace_range(0..1, &upper);
    }
    summary
}

/// The status-dot palette, as oklch triples (L, C, H°).
///
/// Colors live here rather than in either viewport because the *meaning* of a
/// dot must not differ between surfaces — a session that reads "running" in the
/// desktop app cannot read "error" in the terminal. Each frontend converts to
/// its own color type; `comet-ui` has the oklch→sRGB math, `comet-tui` pins the
/// converted values with a test.
pub mod dot {
    /// Running. Pink, not amber: the harsh yellow read as a warning, and running
    /// is routine (user request).
    pub const WORKING: (f32, f32, f32) = (0.718, 0.202, 349.761);
    /// Asking a question. Indigo — must read differently from "busy" at a glance.
    pub const AWAITING: (f32, f32, f32) = (0.673, 0.182, 276.935);
    /// Errored. Red-400.
    pub const ERRORED: (f32, f32, f32) = (0.704, 0.191, 22.216);
    /// Finished but unseen. Emerald — reads as "ready for you".
    pub const COMPLETED: (f32, f32, f32) = (0.765, 0.177, 163.223);
}

// ---------------------------------------------------------------------------
// Checkout selection (new sessions)
// ---------------------------------------------------------------------------

/// Where a new session runs (t3code's env-mode: `local | worktree`).
///
/// "Current worktree" is deliberately **not** a third mode — it is `Local` when
/// the picked ref already happens to be materialized as a worktree, in which
/// case the session reuses that checkout's path. Modelling it as three states
/// would let the UI hold a combination the engine cannot honour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The project's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the project folder as-is.
    CurrentCheckout,
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (the engine mints a `comet/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the project
    /// folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// Resolve the on-send action from the mode and the picked ref.
pub fn checkout_plan(kind: CheckoutKind, picked: Option<&crate::RepoRef>) -> CheckoutPlan {
    let name = picked.map(|r| r.name.clone());
    match kind {
        CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree { base: name },
        CheckoutKind::Local => match picked.and_then(|r| r.worktree_path.clone()) {
            Some(path) => CheckoutPlan::ReuseWorktree {
                path,
                branch: name.unwrap_or_default(),
            },
            None => CheckoutPlan::CurrentCheckout,
        },
    }
}

/// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel`).
pub fn checkout_label(kind: CheckoutKind, picked: Option<&crate::RepoRef>) -> &'static str {
    match kind {
        CheckoutKind::NewWorktree => "New worktree",
        CheckoutKind::Local => {
            if picked.is_some_and(|r| r.worktree_path.is_some()) {
                "Current worktree"
            } else {
                "Current checkout"
            }
        }
    }
}

#[cfg(test)]
mod checkout_tests {
    use super::*;
    use crate::RepoRef;

    fn plain(name: &str) -> RepoRef {
        RepoRef {
            name: name.into(),
            current: false,
            worktree_path: None,
        }
    }

    fn materialized(name: &str, path: &str) -> RepoRef {
        RepoRef {
            name: name.into(),
            current: false,
            worktree_path: Some(path.into()),
        }
    }

    #[test]
    fn local_resolves_by_whether_the_ref_has_a_worktree() {
        // The same mode means two different things depending on the ref — which
        // is exactly why "current worktree" is not its own state.
        assert_eq!(
            checkout_plan(CheckoutKind::Local, Some(&plain("main"))),
            CheckoutPlan::CurrentCheckout
        );
        assert_eq!(
            checkout_plan(CheckoutKind::Local, Some(&materialized("feat", "/wt/feat"))),
            CheckoutPlan::ReuseWorktree {
                path: "/wt/feat".into(),
                branch: "feat".into()
            }
        );
        // No ref picked at all is still the project folder.
        assert_eq!(
            checkout_plan(CheckoutKind::Local, None),
            CheckoutPlan::CurrentCheckout
        );
    }

    #[test]
    fn new_worktree_carries_its_base_and_tolerates_none() {
        assert_eq!(
            checkout_plan(CheckoutKind::NewWorktree, Some(&plain("main"))),
            CheckoutPlan::NewWorktree {
                base: Some("main".into())
            }
        );
        // Refs never loaded: send falls back to the project folder rather than
        // failing, so the base is allowed to be absent.
        assert_eq!(
            checkout_plan(CheckoutKind::NewWorktree, None),
            CheckoutPlan::NewWorktree { base: None }
        );
    }

    #[test]
    fn labels_say_which_of_the_three_outcomes_you_will_get() {
        assert_eq!(
            checkout_label(CheckoutKind::Local, Some(&plain("main"))),
            "Current checkout"
        );
        assert_eq!(
            checkout_label(CheckoutKind::Local, Some(&materialized("f", "/wt/f"))),
            "Current worktree"
        );
        assert_eq!(
            checkout_label(CheckoutKind::NewWorktree, Some(&plain("main"))),
            "New worktree"
        );
    }
}

#[cfg(test)]
mod command_names_tests {
    use super::command_names;

    #[test]
    fn plain_commands_name_themselves() {
        assert_eq!(command_names("git log --oneline"), ["git"]);
        assert_eq!(command_names("/usr/bin/git status"), ["git"]);
        assert_eq!(command_names("cargo test -p comet-ui"), ["cargo"]);
    }

    #[test]
    fn chains_split_on_unquoted_separators() {
        assert_eq!(
            command_names("git log --oneline && git merge-base HEAD origin/main"),
            ["git", "git"]
        );
        assert_eq!(
            command_names("cargo build; cargo test || echo no"),
            ["cargo", "cargo", "echo"]
        );
        assert_eq!(
            command_names("grep -rn \"veil\" crates/ui/src | wc -l"),
            ["grep", "wc"]
        );
        assert_eq!(command_names("set -e\nfixture=0\ngrep -c \"x\""), ["grep"]);
    }

    #[test]
    fn quoted_separators_do_not_split() {
        assert_eq!(command_names("grep -rn \"a; b\" ."), ["grep"]);
        assert_eq!(command_names("awk '{print $1}' f"), ["awk"]);
        assert_eq!(command_names("echo 'a && b'"), ["echo"]);
    }

    #[test]
    fn prefixes_and_wrappers_are_stepped_through() {
        assert_eq!(command_names("FOO=bar BAZ=1 cargo build"), ["cargo"]);
        assert_eq!(command_names("sudo systemctl restart x"), ["systemctl"]);
        assert_eq!(command_names("env FOO=1 make"), ["make"]);
        // Assignments alone parse to nothing — the caller shows a bare count.
        assert!(command_names("fixture_in_original=0").is_empty());
    }

    #[test]
    fn shell_payloads_are_parsed_one_level_deep() {
        assert_eq!(
            command_names("bash -c \"git status && cargo test\""),
            ["git", "cargo"]
        );
        assert_eq!(command_names("sh -lc 'npm run build'"), ["npm"]);
        assert_eq!(command_names("bash scripts/deploy.sh"), ["deploy.sh"]);
        // The shell itself never surfaces as the name.
        assert!(!command_names("bash -c \"true\"").contains(&"bash".to_string()));
    }
}

#[cfg(test)]
mod group_summary_tests {
    use super::*;
    use crate::ToolCall;

    fn exec(c: &str) -> (ToolCall, bool) {
        (ToolCall::Exec { command: c.into() }, false)
    }
    fn read(p: &str) -> (ToolCall, bool) {
        (ToolCall::ReadFile { path: p.into() }, false)
    }

    #[test]
    fn commands_name_themselves_in_the_summary() {
        // Names, not a bare count — the summary is the only line a collapsed
        // group shows, so "Ran 3 commands" would hide what actually ran.
        assert_eq!(
            tool_group_summary(&[exec("cargo test"), exec("git status")]),
            "Ran cargo, git"
        );
        // Repeats collapse to one name, and the count returns to say how many.
        assert_eq!(
            tool_group_summary(&[exec("git add ."), exec("git commit"), exec("git push")]),
            "Ran git (3)"
        );
        // Unparseable commands keep the old bare-count shape.
        assert_eq!(tool_group_summary(&[exec("FOO=1")]), "Ran 1 command");
    }

    #[test]
    fn segments_join_in_kind_order_with_failures_last() {
        let tools = [
            read("a.rs"),
            exec("cargo build"),
            (
                ToolCall::EditFile {
                    path: "b.rs".into(),
                    old_string: None,
                    new_string: None,
                },
                true,
            ),
        ];
        assert_eq!(
            tool_group_summary(&tools),
            "Ran cargo · edited 1 file · read 1 file · 1 failed"
        );
    }

    #[test]
    fn ran_commands_caps_the_name_list() {
        let names = |ns: &[&str]| ns.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(ran_commands(&names(&["cargo"]), 1), "ran cargo");
        assert_eq!(
            ran_commands(&names(&["a", "b", "c", "d"]), 4),
            "ran a, b, c, … (4)"
        );
        assert_eq!(ran_commands(&[], 2), "ran 2 commands");
    }
}

#[cfg(test)]
mod coalesce_tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<String> {
        ps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn siblings_fold_into_one_brace_group() {
        assert_eq!(
            coalesce_paths(&paths(&[
                "crates/ui/src/transcript.rs",
                "crates/ui/src/composer.rs"
            ])),
            "crates/ui/src/{transcript.rs,composer.rs}"
        );
        // Groups keep call order; a lone file in its directory stays whole.
        assert_eq!(
            coalesce_paths(&paths(&[
                "foo/bar/buzz.rs",
                "foo/bar/fizz.rs",
                "test/test2"
            ])),
            "foo/bar/{buzz.rs,fizz.rs}, test/test2"
        );
        // An interleaved directory still folds into its first appearance.
        assert_eq!(
            coalesce_paths(&paths(&["a/x", "b/y", "a/z"])),
            "a/{x,z}, b/y"
        );
    }

    #[test]
    fn non_paths_pass_through_untouched() {
        // Patterns and bare names have no parent: braces would be noise.
        assert_eq!(coalesce_paths(&paths(&["veil", "spring"])), "veil, spring");
        assert_eq!(coalesce_paths(&paths(&["a.rs"])), "a.rs");
        // Exact repeats collapse — reading one file twice reads once.
        assert_eq!(coalesce_paths(&paths(&["a/b.rs", "a/b.rs"])), "a/b.rs");
        assert_eq!(coalesce_paths(&[]), "");
    }
}
