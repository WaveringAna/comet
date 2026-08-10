//! Integration coverage for device-addressed RPCs over paired Nova listeners.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a full
// `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use nova_doc::{MessagePart, SessionCommandPayload};
use nova_engine::peer_sync::PeerSync;
use nova_engine::{EngineCore, HarnessRegistry};
use nova_harness::{Harness, HarnessError, RunControls};
use nova_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode,
};
use nova_rpc::methods;

// ---------------------------------------------------------------------------
// Direct listener fixtures
// ---------------------------------------------------------------------------

async fn start_nova_listener(core: &EngineCore) -> (String, tokio::task::JoinHandle<()>) {
    let endpoint = core
        .nova
        .bind_endpoint(false)
        .await
        .expect("bind iroh endpoint");
    let ticket = core.nova.ticket().expect("iroh ticket");
    let service = core.rpc_service();
    let trust = core.nova.trust();
    let identity = core.nova.identity();
    let pairing = core.nova.pairing();
    let task = tokio::spawn(nova_network::transport::serve_iroh_endpoint(
        endpoint, service, trust, identity, pairing,
    ));
    (ticket, task)
}

async fn pair_engines(
    core_a: &EngineCore,
    _endpoint_a: &str,
    core_b: &EngineCore,
    endpoint_b: &str,
) {
    let pairing = core_b.nova.begin_pairing();
    let code = pairing["code"].as_str().expect("pairing code").to_string();
    core_a
        .nova
        .pair_peer(endpoint_b, &code)
        .await
        .expect("pair A with B");
}

// ---------------------------------------------------------------------------
// Engine fixtures
// ---------------------------------------------------------------------------

/// Instant mock harness so a forwarded QueueCommand fully executes on the target.
struct InstantHarness;

#[async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-1".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "remote reply".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-1".into()),
            }),
        ])
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness));
    Arc::new(registry)
}

fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble_on_port(dir, registry(), HarnessId::Mock, 0).expect("engine assembles")
}

async fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_device_id_routes_over_paired_nova_engines() {
    let dirs = tempfile::tempdir().expect("tempdir");

    let core_b = assemble(&dirs.path().join("b"), "device-b");
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    let (endpoint_a, listener_a) = start_nova_listener(&core_a).await;
    let (endpoint_b, listener_b) = start_nova_listener(&core_b).await;
    pair_engines(&core_a, &endpoint_a, &core_b, &endpoint_b).await;

    // Seed a transcript on B only — proves reads come from B, not A's (empty) doc.
    let handle_b = core_b.doc_host.open("chat-remote").expect("open chat on B");
    handle_b
        .write_user_message("m-b-1", "hello from B", 1_000)
        .expect("write user message");

    let client = nova_rpc::memory_client(core_a.rpc_service());

    // Our own id in targetDeviceId: handled locally, no forward.
    let local = client
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-a" }),
        )
        .await
        .expect("local list");
    assert!(local.is_array());

    // Unary forwarding works in both directions over the paired listeners.
    let remote = client
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-b" }),
        )
        .await
        .expect("direct call from A to B");
    assert!(remote.is_array());
    let client_b = nova_rpc::memory_client(core_b.rpc_service());
    let reverse = client_b
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-a" }),
        )
        .await
        .expect("direct call from B to A");
    assert!(reverse.is_array());

    // The add-project picker's exact call: browse a folder ON B from A's IPC
    // surface (ListFolders + targetDeviceId, direct-peer-forwarded).
    let browse_dir = dirs.path().join("b-folders");
    std::fs::create_dir_all(browse_dir.join("project-x")).expect("browse fixture");
    let listing = client
        .call(
            methods::LIST_FOLDERS,
            serde_json::json!({
                "path": browse_dir.to_string_lossy(),
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("direct ListFolders");
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        names.contains(&"project-x"),
        "remote folder listing must come from B's filesystem: {names:?}"
    );

    // Streaming proxy: WatchDocMessages against B's doc from A's IPC surface.
    let mut stream = client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": "chat-remote", "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote subscribe");
    // The watch emits its current value first ([] if B's publish pass hasn't run yet),
    // then re-emits on every doc change — read until B's entry arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let item = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .expect("remote transcript before timeout")
            .expect("stream alive");
        if item.to_string().contains("hello from B") {
            break;
        }
    }

    // Unary forward with side effects: QueueCommand lands (and executes) on B.
    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: RunRequest {
            prompt: "run remotely".into(),
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: "/tmp".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
        },
        message_id: "m-a-1".into(),
    })
    .expect("serialize command");
    let queued = client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-remote",
                "targetDeviceId": "device-b",
                "command": command,
            }),
        )
        .await
        .expect("queue on B");
    let command_id = queued["commandId"]
        .as_str()
        .expect("command id")
        .to_string();
    let commands = handle_b.doc().read_commands().expect("read B commands");
    assert!(
        commands.iter().any(|c| c.id == command_id),
        "command must live in B's doc"
    );

    // Pi configuration uses the same device selector and is deliberately available
    // to an admin peer. Project scope keeps the test isolated from the user's config.
    let pi_project = dirs.path().join("b-pi-project");
    std::fs::create_dir_all(&pi_project).expect("pi project fixture");
    let pi = client
        .call(
            methods::SET_PI_SETTING,
            serde_json::json!({
                "scope": "project",
                "projectPath": pi_project,
                "key": "transport",
                "value": "sse",
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("configure Pi on B");
    assert_eq!(pi["settings"]["transport"], "sse");
    assert!(pi_project.join(".pi/settings.json").exists());

    listener_a.abort();
    listener_b.abort();
    core_a.shutdown().await;
    core_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_engines_converge_workspace_and_chat_docs_directly() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    let core_b = assemble(&dirs.path().join("b"), "device-b");
    let (endpoint_a, listener_a) = start_nova_listener(&core_a).await;
    let (endpoint_b, listener_b) = start_nova_listener(&core_b).await;
    pair_engines(&core_a, &endpoint_a, &core_b, &endpoint_b).await;

    core_a
        .workspace
        .create_project("project-a", "device-a", "/tmp/a", None, false)
        .expect("project A");
    core_a
        .workspace
        .create_chat("chat-a", "project-a", None, None)
        .expect("chat A");
    core_a
        .doc_host
        .open("chat-a")
        .expect("open A chat")
        .write_user_message("message-a", "from A", 1_000)
        .expect("write A transcript");

    core_b
        .workspace
        .create_project("project-b", "device-b", "/tmp/b", None, false)
        .expect("project B");
    core_b
        .workspace
        .create_chat("chat-b", "project-b", None, None)
        .expect("chat B");
    core_b
        .doc_host
        .open("chat-b")
        .expect("open B chat")
        .write_user_message("message-b", "from B", 2_000)
        .expect("write B transcript");

    PeerSync::new(
        core_a.nova.clone(),
        core_a.workspace.clone(),
        core_a.doc_host.clone(),
    )
    .sync_peer("device-b")
    .await
    .expect("direct sync exchange");

    for core in [&core_a, &core_b] {
        let project_ids: Vec<_> = core
            .workspace
            .read_projects()
            .expect("projects")
            .into_iter()
            .map(|project| project.id)
            .collect();
        assert!(project_ids.contains(&"project-a".to_string()));
        assert!(project_ids.contains(&"project-b".to_string()));
        assert!(
            core.doc_host
                .open("chat-a")
                .expect("synced A chat")
                .doc()
                .read_entries()
                .expect("A entries")
                .iter()
                .any(|entry| entry.id == "message-a")
        );
        assert!(
            core.doc_host
                .open("chat-b")
                .expect("synced B chat")
                .doc()
                .read_entries()
                .expect("B entries")
                .iter()
                .any(|entry| entry.id == "message-b")
        );
    }

    // The UI workflow originates on A but picks B in the add-project palette:
    // A writes a B-owned project/chat and queues the first run locally. One
    // direct sync delivers the workspace row before the command doc, so only B
    // executes it; the next exchange carries transcript and session status back.
    core_a
        .workspace
        .create_project("project-on-b", "device-b", "/tmp", None, false)
        .expect("remote-hosted project created from A");
    core_a
        .workspace
        .create_chat("chat-on-b", "project-on-b", None, None)
        .expect("remote-hosted chat created from A");
    core_a
        .doc_host
        .queue_command(
            "chat-on-b",
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: "start on B".into(),
                    model: None,
                    reasoning: None,
                    model_options: serde_json::Map::new(),
                    cwd: "/tmp".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    attachments: Vec::new(),
                    resume: None,
                },
                message_id: "message-on-b".into(),
            },
        )
        .expect("queue remote-hosted run from A");

    PeerSync::new(
        core_a.nova.clone(),
        core_a.workspace.clone(),
        core_a.doc_host.clone(),
    )
    .sync_peer("device-b")
    .await
    .expect("deliver remote-hosted run to B");

    wait_for(
        || {
            core_b
                .doc_host
                .open("chat-on-b")
                .expect("B chat")
                .doc()
                .read_entries()
                .unwrap_or_default()
                .iter()
                .flat_map(|entry| &entry.parts)
                .any(
                    |part| matches!(part, MessagePart::Text { text, .. } if text == "remote reply"),
                )
        },
        "remote run on B",
    )
    .await;

    PeerSync::new(
        core_a.nova.clone(),
        core_a.workspace.clone(),
        core_a.doc_host.clone(),
    )
    .sync_peer("device-b")
    .await
    .expect("sync B's result back to A");
    assert!(
        core_a
            .doc_host
            .open("chat-on-b")
            .expect("A chat")
            .doc()
            .read_entries()
            .expect("A transcript")
            .iter()
            .flat_map(|entry| &entry.parts)
            .any(|part| matches!(part, MessagePart::Text { text, .. } if text == "remote reply")),
        "A must observe B's transcript"
    );
    assert!(
        core_a
            .workspace
            .doc()
            .read_sessions()
            .expect("A session rows")
            .iter()
            .any(|session| {
                session.chat_id == "chat-on-b"
                    && session.device_id == "device-b"
                    && session.status == SessionStatus::Idle
            }),
        "A must observe B's final session status"
    );

    listener_a.abort();
    listener_b.abort();
    core_a.shutdown().await;
    core_b.shutdown().await;
}

/// M5: terminals are device-addressable — OpenTerminal/WriteTerminal forward as
/// unary calls and SubscribeTerminal proxies its stream through Nova.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_stream_proxies_over_nova() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let dirs = tempfile::tempdir().expect("tempdir");
    let cwd = dirs.path().join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");

    // Engine B's chat row (via its project) pins the terminal cwd.
    let core_b = assemble(&dirs.path().join("b"), "device-b");
    core_b
        .workspace
        .create_project(
            "project-term",
            "device-b",
            &cwd.to_string_lossy(),
            None,
            false,
        )
        .expect("project row on B");
    core_b
        .workspace
        .create_chat("chat-term", "project-term", None, None)
        .expect("chat row on B");
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    let (endpoint_a, listener_a) = start_nova_listener(&core_a).await;
    let (endpoint_b, listener_b) = start_nova_listener(&core_b).await;
    pair_engines(&core_a, &endpoint_a, &core_b, &endpoint_b).await;
    let client = nova_rpc::memory_client(core_a.rpc_service());

    let session = client
        .call(
            methods::OPEN_TERMINAL,
            serde_json::json!({
                "chatId": "chat-term",
                "cols": 80,
                "rows": 24,
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("open terminal on B");
    let terminal_id = session["id"].as_str().expect("terminal id").to_string();
    assert_eq!(
        session["cwd"].as_str(),
        Some(&*cwd.to_string_lossy()),
        "cwd from B's chat row"
    );

    // SubscribeTerminal: the stream is proxied item-by-item over the direct peer link.
    let mut stream = client
        .subscribe(
            methods::SUBSCRIBE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id, "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote subscribe");
    client
        .call(
            methods::WRITE_TERMINAL,
            serde_json::json!({
                "terminalId": terminal_id,
                "data": BASE64.encode("echo nova-$((20+2))\n"),
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("remote write");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut transcript = Vec::new();
    loop {
        let item = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .expect("proxied terminal output before timeout")
            .expect("stream alive");
        if item["type"] == "data" {
            let bytes = BASE64
                .decode(item["data"].as_str().expect("data"))
                .expect("valid base64");
            transcript.extend(bytes);
        }
        if String::from_utf8_lossy(&transcript).contains("nova-22") {
            break;
        }
    }

    client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id, "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote close");

    listener_a.abort();
    listener_b.abort();
    core_a.shutdown().await;
    core_b.shutdown().await;
}

#[tokio::test]
async fn unpaired_remote_target_fails_clearly() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let core = assemble(&dirs.path().join("solo"), "device-solo");
    let client = nova_rpc::memory_client(core.rpc_service());
    let err = client
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-elsewhere" }),
        )
        .await
        .expect_err("offline forward must fail");
    assert!(err.to_string().contains("is not paired"), "got: {err}");
    core.shutdown().await;
}
