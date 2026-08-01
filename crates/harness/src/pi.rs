//! pi harness: one long-lived `pi --mode rpc` child per Nova session.
//!
//! pi's documented RPC mode is strict LF-delimited JSONL. the child is started
//! with the session's cwd, receives prompts and steering commands over stdin,
//! and emits agent/tool lifecycle events over stdout. this adapter deliberately
//! translates only the stable RPC events the frontend needs; pi remains the
//! owner of model context, tools, compaction, and session persistence.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    TOOL_OUTPUT_MAX_BYTES, ToolCall, UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls};

fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PI_EXECUTABLE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let exe = if cfg!(windows) { "pi.exe" } else { "pi" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local").join("bin").join(exe));
        candidates.push(home.join(".bun").join("bin").join(exe));
        candidates.push(home.join("Library").join("pnpm").join(exe));
    }
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|dir| dir.join(exe)),
    );
    candidates.into_iter().find(|path| path.is_file())
}

pub struct PiHarness {
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
}

impl Default for PiHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
        }
    }
}

impl PiHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    pub fn with_interrupt_grace(mut self, grace: Duration) -> Self {
        self.interrupt_grace = grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        self.executable.clone().or_else(resolve_pi_executable).ok_or_else(|| {
            HarnessError::NotInstalled(
                "pi (searched PATH, ~/.local/bin, ~/.bun/bin, ~/Library/pnpm, and node version manager bins; set PI_EXECUTABLE to override)".into(),
            )
        })
    }

    fn command(&self, executable: &PathBuf, request: &RunRequest) -> Command {
        let mut command = Command::new(executable);
        crate::prepend_exe_dir_to_path(&mut command, executable);
        command.args(["--mode", "rpc"]);
        // --approve is pi's documented non-interactive project-trust switch;
        // tool execution itself remains governed by pi's RPC runtime.
        if request.auto_approve {
            command.arg("--approve");
        }
        if let Some(model) = request.model.as_deref().filter(|model| !model.is_empty()) {
            command.args(["--model", model]);
        }
        if let Some(thinking) = request.reasoning {
            if let Some(level) = pi_thinking_level(thinking) {
                command.args(["--thinking", level]);
            }
        }
        if let Some(session_id) = request.resume.as_deref().filter(|id| !id.is_empty()) {
            command.args(["--session-id", session_id]);
        }
        if !request.cwd.is_empty() {
            command.current_dir(&request.cwd);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

fn pi_thinking_level(level: ReasoningLevel) -> Option<&'static str> {
    Some(match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh | ReasoningLevel::Ultra | ReasoningLevel::Ultracode => "xhigh",
        ReasoningLevel::Max => "max",
        // pi has no prompt-prefix equivalent; its closest native setting is
        // the highest thinking level available to the selected model.
        ReasoningLevel::Ultrathink => "max",
    })
}

#[async_trait]
impl Harness for PiHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn display_name(&self) -> &str {
        "Pi"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[
            ReasoningLevel::Minimal,
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High,
            ReasoningLevel::XHigh,
            ReasoningLevel::Max,
        ]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let executable = self.resolve_executable()?;
        let mut command = Command::new(&executable);
        crate::prepend_exe_dir_to_path(&mut command, &executable);
        let output = command.arg("--list-models").output().await?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            return Err(HarnessError::Protocol(format!(
                "pi --list-models failed{suffix}"
            )));
        }
        let models = parse_pi_models(&String::from_utf8_lossy(&output.stdout));
        if models.is_empty() {
            return Err(HarnessError::Protocol(
                "pi --list-models returned no models".into(),
            ));
        }
        Ok(order_pi_models(
            models,
            query_pi_default_model(&executable).await,
        ))
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let executable = self.resolve_executable()?;
        let mut child = self
            .command(&executable, &request)
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    HarnessError::NotInstalled(executable.display().to_string())
                } else {
                    HarnessError::Io(error)
                }
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("pi child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::pi", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (event_tx, event_rx) = mpsc::channel(256);
        tokio::spawn(run_session(PiSession {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            request,
            controls,
            event_tx,
            interrupt_grace: self.interrupt_grace,
            stderr_tail,
        }));
        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

struct PiSession {
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    request: RunRequest,
    controls: RunControls,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    interrupt_grace: Duration,
    stderr_tail: crate::StderrTail,
}

async fn write_command(stdin: &mut ChildStdin, command: Value) -> Result<(), HarnessError> {
    let mut line = serde_json::to_vec(&command)
        .map_err(|error| HarnessError::Protocol(format!("serialize pi command: {error}")))?;
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await?;
    Ok(())
}

async fn send_event(
    tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    event: AgentEvent,
) -> bool {
    tx.send(Ok(event)).await.is_ok()
}

async fn run_session(session: PiSession) {
    let PiSession {
        mut child,
        mut stdin,
        mut stdout,
        request,
        controls,
        event_tx,
        interrupt_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);
    let state_id = "nova-state";
    let prompt_id = "nova-prompt";
    if let Err(error) =
        write_command(&mut stdin, json!({ "id": state_id, "type": "get_state" })).await
    {
        let _ = event_tx.send(Err(error)).await;
        return;
    }

    let mut session_id = request.resume.clone().unwrap_or_default();
    let mut assistant_message_id = uuid::Uuid::new_v4().to_string();
    let mut started = false;
    let mut prompt_sent = false;
    let mut streaming = false;
    let mut interrupted = false;
    let mut prompt_error: Option<String> = None;
    let mut last_error: Option<String> = None;
    // pi streams assistant text as deltas and repeats the complete message in
    // `message_end`. remember whether the current assistant message already
    // produced a delta so the completion frame cannot append it a second time.
    let mut assistant_text_emitted = false;

    'main: loop {
        tokio::select! {
            line = stdout.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim_end_matches('\r');
                    if line.is_empty() { continue; }
                    let value: Value = match serde_json::from_str(line) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::debug!(target: "comet_harness::pi", "skipping invalid rpc line: {error}");
                            continue;
                        }
                    };
                    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "response" {
                        let id = value.get("id").and_then(Value::as_str).unwrap_or("");
                        if id == state_id {
                            if value.get("success").and_then(Value::as_bool) != Some(true) {
                                prompt_error = Some(value.get("error").and_then(Value::as_str).unwrap_or("pi get_state failed").into());
                            } else if let Some(data) = value.get("data") {
                                session_id = data.get("sessionId").and_then(Value::as_str).unwrap_or(&session_id).to_owned();
                            }
                            if !started {
                                started = true;
                                let model = request.model.clone().unwrap_or_default();
                                if !send_event(&event_tx, AgentEvent::SessionStarted {
                                    harness: HarnessId::Pi,
                                    model,
                                    tools: vec!["read".into(), "bash".into(), "edit".into(), "write".into()],
                                    cwd: request.cwd.clone(),
                                    session_id: session_id.clone(),
                                    assistant_message_id: assistant_message_id.clone(),
                                }).await { break 'main; }
                                if prompt_error.is_none() {
                                    if let Err(error) = write_command(&mut stdin, json!({
                                        "id": prompt_id,
                                        "type": "prompt",
                                        "message": prompt_text(&request),
                                    })).await {
                                        prompt_error = Some(error.to_string());
                                    } else {
                                        prompt_sent = true;
                                    }
                                }
                                if let Some(error) = prompt_error.take() {
                                    let _ = send_event(&event_tx, AgentEvent::Error { message: error.clone() }).await;
                                    let _ = send_event(&event_tx, AgentEvent::Done { status: DoneStatus::Errored, result: None, error: Some(error), session_id: (!session_id.is_empty()).then_some(session_id.clone()) }).await;
                                    break 'main;
                                }
                            }
                        } else if value.get("success").and_then(Value::as_bool) == Some(false) {
                            if id == prompt_id {
                                let error = value.get("error").and_then(Value::as_str).unwrap_or("pi rejected the prompt").to_owned();
                                let _ = send_event(&event_tx, AgentEvent::Error { message: error.clone() }).await;
                                let _ = send_event(&event_tx, AgentEvent::Done { status: DoneStatus::Errored, result: None, error: Some(error), session_id: (!session_id.is_empty()).then_some(session_id.clone()) }).await;
                                break 'main;
                            }
                        }
                        continue;
                    }

                    if kind == "extension_ui_request" {
                        if !handle_extension_request(&mut stdin, &value, &request_input, &event_tx).await {
                            break 'main;
                        }
                        continue;
                    }
                    match kind {
                        "agent_start" => { streaming = true; last_error = None; }
                        "agent_end" => {
                            // pi documents `agent_end` as the end of one low-level
                            // attempt. retries, compaction retries, and queued
                            // follow-ups can still happen before `agent_settled`.
                            // An interrupt is the one exception: settle it now so
                            // cancellation does not wait on a follow-up.
                            streaming = false;
                            if interrupted {
                                let status = DoneStatus::Interrupted;
                                if !send_event(&event_tx, AgentEvent::Done { status, result: None, error: last_error.take(), session_id: (!session_id.is_empty()).then_some(session_id.clone()) }).await { break 'main; }
                                break 'main;
                            }
                        }
                        "agent_settled" => {
                            let status = if interrupted { DoneStatus::Interrupted } else if last_error.is_some() { DoneStatus::Errored } else { DoneStatus::Completed };
                            if !send_event(&event_tx, AgentEvent::Done { status, result: None, error: last_error.take(), session_id: (!session_id.is_empty()).then_some(session_id.clone()) }).await { break 'main; }
                            break 'main;
                        }
                        "message_start" => {
                            if value.get("message").and_then(|m| m.get("role")).and_then(Value::as_str) == Some("assistant") {
                                assistant_message_id = value.get("message").and_then(|m| m.get("id")).and_then(Value::as_str).unwrap_or(&assistant_message_id).to_owned();
                                assistant_text_emitted = false;
                            }
                        }
                        "message_update" => {
                            let event = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
                            match event.get("type").and_then(Value::as_str).unwrap_or("") {
                                "text_delta" => {
                                    if let Some(text) = event.get("delta").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                        assistant_text_emitted = true;
                                        if !send_event(&event_tx, AgentEvent::TextDelta { text: text.into() }).await { break 'main; }
                                    }
                                }
                                "thinking_delta" => if let Some(text) = event.get("delta").and_then(Value::as_str).filter(|text| !text.is_empty()) { if !send_event(&event_tx, AgentEvent::ReasoningDelta { text: text.into() }).await { break 'main; } },
                                "error" => { let message = event.get("error").and_then(|e| e.get("errorMessage")).and_then(Value::as_str).or_else(|| event.get("errorMessage").and_then(Value::as_str)).unwrap_or("pi assistant error").to_owned(); last_error = Some(message.clone()); if !send_event(&event_tx, AgentEvent::Error { message }).await { break 'main; } }
                                _ => {}
                            }
                        }
                        "message_end" => {
                            let message = value.get("message").unwrap_or(&Value::Null);
                            let id = message.get("id").and_then(Value::as_str).unwrap_or(&assistant_message_id).to_owned();
                            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                                if !assistant_text_emitted {
                                    if let Some(text) = message_text(message) { if !send_event(&event_tx, AgentEvent::TextDelta { text }).await { break 'main; } }
                                }
                                assistant_text_emitted = false;
                                if let Some(error) = message_error(message) {
                                    last_error = Some(error.clone());
                                    if !send_event(&event_tx, AgentEvent::Error { message: error }).await { break 'main; }
                                } else if !send_event(&event_tx, AgentEvent::AssistantMessageCompleted { assistant_message_id: id }).await { break 'main; }
                            }
                        }
                        "tool_execution_start" => {
                            let id = value.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_owned();
                            let name = value.get("toolName").and_then(Value::as_str).unwrap_or("unknown");
                            let args = value.get("args").cloned().unwrap_or(Value::Null);
                            if !send_event(&event_tx, AgentEvent::ToolCall { id, call: pi_tool_call(name, &args) }).await { break 'main; }
                        }
                        "tool_execution_end" => {
                            let id = value.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_owned();
                            let is_error = value.get("isError").and_then(Value::as_bool).unwrap_or(false);
                            let (output, output_truncated) = tool_result_output(value.get("result").unwrap_or(&Value::Null));
                            if !send_event(&event_tx, AgentEvent::ToolResult { id, is_error, output, output_truncated }).await { break 'main; }
                        }
                        "error" => {
                            let message = value.get("message").and_then(Value::as_str).unwrap_or("pi error").to_owned();
                            last_error = Some(message.clone());
                            if !send_event(&event_tx, AgentEvent::Error { message }).await { break 'main; }
                        }
                        _ => {}
                    }
                }
                Ok(None) => {
                    if !interrupted && prompt_sent {
                        let error = crate::crash_message("pi", child.try_wait().ok().flatten(), &stderr_tail);
                        let _ = send_event(&event_tx, AgentEvent::Error { message: error.clone() }).await;
                        let _ = send_event(&event_tx, AgentEvent::Done { status: DoneStatus::Errored, result: None, error: Some(error), session_id: (!session_id.is_empty()).then_some(session_id.clone()) }).await;
                    }
                    break 'main;
                }
                Err(error) => { let _ = event_tx.send(Err(HarnessError::Io(error))).await; break 'main; }
            },
            steer = steering.recv() => match steer {
                Some(message) => {
                    let command_type = if streaming { "steer" } else { "prompt" };
                    if write_command(&mut stdin, json!({ "type": command_type, "message": prompt_text_from_str(&message.prompt), "streamingBehavior": "steer" })).await.is_err() { break 'main; }
                }
                None => break 'main,
            },
            _ = interrupt.cancelled(), if !interrupted => {
                interrupted = true;
                let _ = write_command(&mut stdin, json!({ "type": "abort" })).await;
                if let Some(pid) = child.id() {
                    tokio::spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        #[cfg(unix)] unsafe { libc::kill(pid as i32, libc::SIGTERM); }
                    });
                }
            }
        }
    }
    if !child.id().is_none() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

fn prompt_text(request: &RunRequest) -> String {
    let mut prompt = request.prompt.clone();
    if !request.attachments.is_empty() {
        prompt.push_str("\n\nattached local files:\n");
        for path in &request.attachments {
            prompt.push_str("- ");
            prompt.push_str(path);
            prompt.push('\n');
        }
    }
    prompt_text_from_str(&prompt)
}

fn prompt_text_from_str(text: &str) -> String {
    text.to_owned()
}

async fn query_pi_default_model(executable: &PathBuf) -> Option<String> {
    let mut command = Command::new(executable);
    crate::prepend_exe_dir_to_path(&mut command, executable);
    let mut child = command
        .args(["--mode", "rpc", "--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    write_command(
        &mut stdin,
        json!({ "id": "nova-model-state", "type": "get_state" }),
    )
    .await
    .ok()?;
    let mut lines = BufReader::new(stdout).lines();
    let model = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(line)) = lines.next_line().await {
            let value: Value = serde_json::from_str(&line).ok()?;
            if value.get("type").and_then(Value::as_str) != Some("response")
                || value.get("id").and_then(Value::as_str) != Some("nova-model-state")
                || value.get("success").and_then(Value::as_bool) != Some(true)
            {
                continue;
            }
            return value
                .get("data")
                .and_then(|data| data.get("model"))
                .and_then(|model| model.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        None
    })
    .await
    .ok()
    .flatten();
    let _ = child.start_kill();
    let _ = child.wait().await;
    model
}

fn order_pi_models(mut models: Vec<Model>, default_id: Option<String>) -> Vec<Model> {
    let Some(default_id) = default_id else {
        return models;
    };
    let position = models.iter().position(|model| {
        model.id == default_id
            || model
                .id
                .strip_prefix("openai-codex/")
                .is_some_and(|id| id == default_id)
    });
    if let Some(position) = position.filter(|position| *position > 0) {
        let model = models.remove(position);
        models.insert(0, model);
    }
    models
}

fn message_error(message: &Value) -> Option<String> {
    message
        .get("errorMessage")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            (message.get("stopReason").and_then(Value::as_str) == Some("error"))
                .then(|| "pi assistant error".to_owned())
        })
}

fn message_text(message: &Value) -> Option<String> {
    let content = message.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn parse_pi_models(output: &str) -> Vec<Model> {
    let mut models = Vec::new();
    for line in output.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 2 || columns[0] == "provider" || columns[0].starts_with('-') {
            continue;
        }
        let provider = columns[0];
        let model_name = columns[1];
        if provider == "No" || model_name == "models" {
            continue;
        }
        let id = format!("{provider}/{model_name}");
        if models.iter().any(|model: &Model| model.id == id) {
            continue;
        }
        let thinking = columns.get(4).is_some_and(|value| *value == "yes");
        let reasoning_levels = if thinking {
            PiHarness::default().reasoning_levels().to_vec()
        } else {
            Vec::new()
        };
        models.push(Model {
            id,
            label: model_name.to_owned(),
            description: Some(provider.to_owned()),
            reasoning_levels,
            options: vec![],
        });
    }
    models
}

/// Displayable output text from pi's `tool_execution_end` `result`, per
/// pi-mono's schema: `AgentToolResult { content: [{type:"text", text} |
/// {type:"image", …}], details }` (normalized since pi-ai's "Normalized
/// tool_execution_end result" change; older frames carried a bare string).
/// Text blocks join with newlines; images don't render in a command feed.
/// Tail-capped at [`TOOL_OUTPUT_MAX_BYTES`] — the run journal is append-only,
/// so unbounded build logs would grow it without limit.
fn tool_result_output(result: &Value) -> (Option<String>, bool) {
    let text = match result {
        Value::String(text) => Some(text.clone()),
        Value::Object(_) => result
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        _ => None,
    };
    text.map(tail_cap)
        .map_or((None, false), |(t, capped)| (Some(t), capped))
}

/// Keep the last [`TOOL_OUTPUT_MAX_BYTES`] of `text`, starting on a char
/// boundary; reports whether anything was cut.
fn tail_cap(mut text: String) -> (String, bool) {
    if text.len() <= TOOL_OUTPUT_MAX_BYTES {
        return (text, false);
    }
    let mut start = text.len() - TOOL_OUTPUT_MAX_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (text.split_off(start), true)
}

fn pi_tool_call(name: &str, args: &Value) -> ToolCall {
    let string = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    match name {
        "bash" => ToolCall::Exec {
            command: string("command"),
        },
        "read" => ToolCall::ReadFile {
            path: string("path"),
        },
        "write" => ToolCall::WriteFile {
            path: string("path"),
            content: args
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "edit" => ToolCall::EditFile {
            path: string("path"),
            old_string: args
                .get("oldText")
                .and_then(Value::as_str)
                .map(str::to_owned),
            new_string: args
                .get("newText")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "grep" => ToolCall::Search {
            pattern: string("pattern"),
            path: args.get("path").and_then(Value::as_str).map(str::to_owned),
        },
        "find" => ToolCall::Glob {
            pattern: string("pattern"),
        },
        "ls" => ToolCall::Search {
            pattern: "*".into(),
            path: args.get("path").and_then(Value::as_str).map(str::to_owned),
        },
        _ => ToolCall::Unknown {
            name: name.into(),
            input: (!args.is_null()).then_some(args.clone()),
        },
    }
}

async fn handle_extension_request(
    stdin: &mut ChildStdin,
    request: &Value,
    request_input: &Arc<
        Box<
            dyn Fn(
                    Vec<UserInputQuestion>,
                )
                    -> tokio::sync::oneshot::Receiver<Vec<comet_proto::UserInputAnswer>>
                + Send
                + Sync,
        >,
    >,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
) -> bool {
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let response = match method {
        "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text" => return true,
        "select" => {
            let options = request
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let answers = request_input(vec![UserInputQuestion {
                id: id.clone(),
                header: request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("pi")
                    .into(),
                question: request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("choose an option")
                    .into(),
                options,
                multi_select: false,
            }])
            .await
            .unwrap_or_default();
            json!({ "type": "extension_ui_response", "id": id, "value": answers.first().and_then(|answer| answer.labels.first()).cloned().unwrap_or_default() })
        }
        "confirm" => {
            let answers = request_input(vec![UserInputQuestion {
                id: id.clone(),
                header: "confirm".into(),
                question: request
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("continue?")
                    .into(),
                options: vec!["yes".into(), "no".into()],
                multi_select: false,
            }])
            .await
            .unwrap_or_default();
            let confirmed = answers
                .first()
                .and_then(|answer| answer.labels.first())
                .is_some_and(|label| label.eq_ignore_ascii_case("yes"));
            json!({ "type": "extension_ui_response", "id": id, "confirmed": confirmed })
        }
        "input" | "editor" => {
            let answers = request_input(vec![UserInputQuestion {
                id: id.clone(),
                header: request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("pi")
                    .into(),
                question: request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("enter a value")
                    .into(),
                options: vec![],
                multi_select: false,
            }])
            .await
            .unwrap_or_default();
            json!({ "type": "extension_ui_response", "id": id, "value": answers.first().and_then(|answer| answer.labels.first()).cloned().unwrap_or_default() })
        }
        _ => return true,
    };
    if write_command(stdin, response).await.is_err() {
        return false;
    }
    !event_tx.is_closed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_output_extracts_text_blocks() {
        // pi-mono's documented frame: AgentToolResult with text content.
        let (output, capped) = tool_result_output(&json!({
            "content": [{"type": "text", "text": "total 48\n…"}],
            "details": {"truncation": null}
        }));
        assert_eq!(output.as_deref(), Some("total 48\n…"));
        assert!(!capped);
        // Multiple text blocks join; images drop.
        let (output, _) = tool_result_output(&json!({
            "content": [
                {"type": "text", "text": "a"},
                {"type": "image", "data": "…"},
                {"type": "text", "text": "b"}
            ]
        }));
        assert_eq!(output.as_deref(), Some("a\nb"));
        // Legacy bare-string results still parse.
        let (output, _) = tool_result_output(&json!("plain output"));
        assert_eq!(output.as_deref(), Some("plain output"));
        // Anything else (null, absent content) is no capture.
        assert_eq!(tool_result_output(&Value::Null), (None, false));
        assert_eq!(tool_result_output(&json!({"details": {}})), (None, false));
    }

    #[test]
    fn tail_cap_keeps_suffix_on_char_boundary() {
        let (text, capped) = tail_cap("short".to_owned());
        assert_eq!((text.as_str(), capped), ("short", false));
        // Multibyte chars at the cut point: no panics, valid UTF-8, capped.
        let big = "é".repeat(TOOL_OUTPUT_MAX_BYTES);
        let (text, capped) = tail_cap(big);
        assert!(capped);
        assert!(text.len() <= TOOL_OUTPUT_MAX_BYTES);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn pi_tool_names_map_to_existing_transcript_calls() {
        assert_eq!(
            pi_tool_call("bash", &json!({"command":"cargo test"})),
            ToolCall::Exec {
                command: "cargo test".into()
            }
        );
        assert_eq!(
            pi_tool_call(
                "edit",
                &json!({"path":"src/lib.rs","oldText":"a","newText":"b"})
            ),
            ToolCall::EditFile {
                path: "src/lib.rs".into(),
                old_string: Some("a".into()),
                new_string: Some("b".into())
            }
        );
    }

    #[test]
    fn pi_reasoning_uses_native_levels() {
        assert_eq!(pi_thinking_level(ReasoningLevel::High), Some("high"));
        assert_eq!(pi_thinking_level(ReasoningLevel::Ultrathink), Some("max"));
    }

    #[test]
    fn pi_model_catalog_parses_provider_model_rows() {
        let models = parse_pi_models(
            "provider model context max-out thinking images\nopenai-codex gpt-5.4 272K 128K yes yes\nlocal no-think 32K 8K no no\n",
        );
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openai-codex/gpt-5.4");
        assert_eq!(models[0].label, "gpt-5.4");
        assert_eq!(models[0].description.as_deref(), Some("openai-codex"));
        assert!(!models[0].reasoning_levels.is_empty());
        assert!(models[1].reasoning_levels.is_empty());
    }
}
