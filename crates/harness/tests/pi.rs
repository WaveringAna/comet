//! pi harness integration coverage against the documented rpc jsonl boundary.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{CancellationToken, Harness, PiHarness, RunControls, SteerMessage};
use comet_proto::{AgentEvent, RunRequest, SandboxLevel, UserInputAnswer};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-pi.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn request(cwd: String) -> RunRequest {
    RunRequest {
        prompt: "hello pi".into(),
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd,
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume: None,
        attachments: Vec::new(),
    }
}

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(8);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (tx, rx) = oneshot::channel::<Vec<UserInputAnswer>>();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: interrupt.clone(),
    };
    (controls, steer_tx, interrupt)
}

#[tokio::test]
async fn spawns_one_pi_process_in_the_session_folder_and_maps_rpc_events() {
    let folder = tempfile::tempdir().unwrap();
    let (controls, _steer, interrupt) = controls();
    let harness = PiHarness::new().with_executable(fixture_path());
    let models = harness.models().await.unwrap();
    assert_eq!(models[0].id, "openai-codex/gpt-5.4");
    let mut stream = harness
        .run(request(folder.path().display().to_string()), controls)
        .await
        .unwrap();

    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("fake pi should emit promptly")
            .expect("the session should emit a done event")
            .expect("rpc events should normalize");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    assert!(events.iter().any(|event| matches!(event, AgentEvent::SessionStarted { harness, session_id, .. } if *harness == comet_proto::HarnessId::Pi && session_id == "pi-session-1")));
    let text_events: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text_events.len(),
        1,
        "pi's completion frame must not duplicate deltas"
    );
    assert!(text_events[0].contains(folder.path().to_str().unwrap()));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { assistant_message_id } if assistant_message_id == "m1")));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: comet_proto::DoneStatus::Completed,
            ..
        })
    ));

    interrupt.cancel();
}

#[tokio::test]
async fn delivered_steer_emits_a_boundary_before_second_turn_output() {
    let folder = tempfile::tempdir().unwrap();
    let (controls, steer, interrupt) = controls();
    let harness = PiHarness::new().with_executable(fixture_path());
    let mut run = request(folder.path().display().to_string());
    run.model = Some("steer-model".into());
    let mut stream = harness.run(run, controls).await.unwrap();

    let mut events = Vec::new();
    let mut steer_sent = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("fake pi should emit promptly")
            .expect("the session should emit a done event")
            .expect("rpc events should normalize");
        if matches!(&event, AgentEvent::TextDelta { text } if text == "first turn output")
            && !steer_sent
        {
            steer
                .send(SteerMessage {
                    prompt: "second turn".into(),
                    message_id: Some("user-2".into()),
                })
                .await
                .unwrap();
            steer_sent = true;
        }
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    let first = events
        .iter()
        .position(
            |event| matches!(event, AgentEvent::TextDelta { text } if text == "first turn output"),
        )
        .unwrap();
    let boundary = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Steered { assistant_message_id: Some(id), .. } if id == "m1"))
        .unwrap();
    let second = events
        .iter()
        .position(
            |event| matches!(event, AgentEvent::TextDelta { text } if text == "second turn output"),
        )
        .unwrap();
    assert!(first < boundary && boundary < second);

    interrupt.cancel();
}

#[tokio::test]
async fn assistant_message_errors_are_not_reported_as_empty_successes() {
    let folder = tempfile::tempdir().unwrap();
    let (controls, _steer, interrupt) = controls();
    let harness = PiHarness::new().with_executable(fixture_path());
    let mut run = request(folder.path().display().to_string());
    run.model = Some("error-model".into());
    let mut stream = harness.run(run, controls).await.unwrap();

    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("fake pi should emit promptly")
            .expect("the session should emit a done event")
            .expect("rpc events should normalize");
        let done = matches!(event, AgentEvent::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Error { message } if message == "the selected pi model is unavailable"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::AssistantMessageCompleted { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: comet_proto::DoneStatus::Errored,
            error: Some(error),
            ..
        }) if error == "the selected pi model is unavailable"
    ));

    interrupt.cancel();
}
