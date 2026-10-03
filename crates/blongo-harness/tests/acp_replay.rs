//! Conformance against t3code's recorded ACP agent sessions
//! (tests/fixtures/t3code/acp/*.ndjson: ACP registry recordings and
//! protocol fixtures from t3code, see THIRD_PARTY_NOTICES.md), driven
//! through the Antigravity agent profile. `replay_acp.py` plays the agent.
//!
//! These recordings are not of Antigravity itself (Antigravity's server was
//! unreachable for both projects' recorders); they pin the ACP v1 behaviour
//! the Antigravity profile relies on: prompt/response turn boundaries,
//! streamed chunks and thoughts, tool calls and permission options, cancel,
//! cancel-and-resend steering, plans, prompt errors and the models list.

use std::path::PathBuf;
use std::time::Duration;

use blongo_harness::acp::{self, AcpAgent};
use blongo_harness::{ApprovalDecision, Session, SessionConfig};
use blongo_protocol::{AgentEvent, PlanStatus, TurnStatus};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The fake agents are Python scripts that run through their `#!` line.
/// Windows has no shebangs, so there they run through a `.cmd` wrapper
/// that calls `python`.
fn runnable(script: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let dir = std::env::temp_dir().join(format!("blongo-test-shims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join(script.file_stem().unwrap()).with_extension("cmd");
        if !shim.exists() {
            // Tests run in parallel: write aside, then move into place.
            let tmp = dir.join(format!(
                "{}.{:?}.tmp",
                script.file_stem().unwrap().to_string_lossy(),
                std::thread::current().id()
            ));
            std::fs::write(&tmp, format!("@python \"{}\" %*\r\n", script.display())).unwrap();
            if std::fs::rename(&tmp, &shim).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        shim
    }
    #[cfg(not(windows))]
    script
}

struct Replay {
    transcript: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

impl Drop for Replay {
    fn drop(&mut self) {
        if let Some(dir) = self.log.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Replay {
    fn new(scenario: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "blongo-acp-replay-{scenario}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            transcript: fixtures().join(format!("t3code/acp/{scenario}.ndjson")),
            log: dir.join("log.ndjson"),
            state: dir.join("state"),
        }
    }

    fn agent() -> AcpAgent {
        AcpAgent {
            setup_timeout: Duration::from_secs(10),
            extra_paths: Vec::new(),
            ..acp::antigravity()
        }
    }

    async fn start(&self) -> Session {
        self.start_with(None).await
    }

    async fn start_with(&self, model: Option<&str>) -> Session {
        let mut config = SessionConfig::new(std::env::temp_dir())
            .executable(runnable(fixtures().join("replay_acp.py")))
            .env("ACP_REPLAY_TRANSCRIPT", &self.transcript)
            .env("ACP_REPLAY_LOG", &self.log)
            .env("ACP_REPLAY_STATE", &self.state);
        config.model = model.map(str::to_owned);
        acp::start(config, Self::agent()).await.unwrap()
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn pairs(&self, label: &str) -> Vec<(Value, Value)> {
        self.log()
            .into_iter()
            .filter(|e| e["label"] == label)
            .map(|e| (e["expected"].clone(), e["actual"].clone()))
            .collect()
    }

    fn unexpected(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|e| e.get("unexpected"))
            .map(|f| f["method"].as_str().unwrap_or("<response>").to_owned())
            .collect()
    }

    fn ended(&self) -> bool {
        self.log().iter().any(|e| e["end"] == 0)
    }

    /// The user's text in the n-th recorded prompt (t3code wraps the first
    /// one in its own instructions; Blongo sends the request alone).
    fn prompt(&self, n: usize) -> String {
        let text = std::fs::read_to_string(&self.transcript)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["type"] == "expect_outbound" && e["frame"]["method"] == "session/prompt")
            .nth(n)
            .and_then(|e| {
                e.pointer("/frame/params/prompt/0/text")?
                    .as_str()
                    .map(str::to_owned)
            })
            .expect("recorded prompt");
        match text.split_once("<user_request>\n") {
            Some((_, rest)) => rest.trim_end_matches("\n</user_request>").to_owned(),
            None => text,
        }
    }

    /// Every session/prompt Blongo sent names the recorded session and
    /// carries the recorded request text.
    fn check_prompts(&self) {
        let pairs: Vec<_> = self
            .log()
            .into_iter()
            .filter(|e| e["expected"]["method"] == "session/prompt")
            .collect();
        assert!(!pairs.is_empty());
        for (n, pair) in pairs.iter().enumerate() {
            let (expected, actual) = (&pair["expected"]["params"], &pair["actual"]["params"]);
            assert_eq!(actual["sessionId"], expected["sessionId"]);
            assert_eq!(actual["prompt"][0]["type"], "text");
            assert_eq!(actual["prompt"][0]["text"], self.prompt(n).as_str());
        }
    }
}

async fn next(session: &mut Session) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(20), session.next_event())
        .await
        .expect("timed out")
        .expect("session ended")
}

/// Setup events (`Models`) up to `SessionStarted`; returns the models.
async fn session_started(session: &mut Session) -> Vec<String> {
    let mut models = Vec::new();
    loop {
        match next(session).await {
            AgentEvent::SessionStarted {
                provider_session_id,
            } => {
                assert_eq!(provider_session_id, "acp-replay-session-1");
                return models;
            }
            AgentEvent::Models { models: m } => {
                models.extend(m.into_iter().map(|m| format!("{}={}", m.id, m.label)));
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
    }
}

async fn turn(
    session: &mut Session,
    mut on_event: impl FnMut(&Session, &AgentEvent),
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = next(session).await;
        on_event(session, &event);
        let done = matches!(event, AgentEvent::TurnCompleted { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

fn text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn status(events: &[AgentEvent]) -> TurnStatus {
    match events.last() {
        Some(AgentEvent::TurnCompleted { status }) => *status,
        other => panic!("turn did not complete: {other:?}"),
    }
}

async fn shutdown(session: Session) {
    tokio::time::timeout(Duration::from_secs(10), session.shutdown())
        .await
        .expect("shutdown timed out");
}

#[tokio::test]
async fn simple_text_and_reasoning() {
    let replay = Replay::new("simple");
    let mut session = replay.start().await;
    let models = session_started(&mut session).await;
    assert_eq!(
        models,
        vec![
            "recorded-model-a=Recorded Model A",
            "recorded-model-b=Recorded Model B"
        ]
    );
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(text(&events), "fixture simple ok");
    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "I should answer exactly.");
    shutdown(session).await;
    replay.check_prompts();
    assert!(replay.ended());
    // The recording advertises only a "replay" auth method, so the
    // Antigravity profile does not call `authenticate`.
    assert!(replay.unexpected().is_empty(), "{:?}", replay.unexpected());
    let (_, init) = replay.pairs("initialize").swap_remove(0);
    assert_eq!(init["params"]["protocolVersion"], 1);
    let (_, new) = replay.pairs("session.new").swap_remove(0);
    assert_eq!(new["params"]["mcpServers"], serde_json::json!([]));
}

#[tokio::test]
async fn multi_turn() {
    let replay = Replay::new("multi_turn");
    let mut session = replay.start().await;
    session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let first = turn(&mut session, |_, _| {}).await;
    assert_eq!(text(&first), "first fixture turn complete");
    session.prompt(replay.prompt(1)).unwrap();
    let second = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&second), TurnStatus::Completed);
    assert_eq!(text(&second), "second fixture turn complete");
    shutdown(session).await;
    replay.check_prompts();
    assert!(replay.unexpected().is_empty());
}

#[tokio::test]
async fn permission_allow_once() {
    let replay = Replay::new("tool_call_read_only_on_request");
    let mut session = replay.start().await;
    session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let mut asked = 0;
    let events = turn(&mut session, |s, e| {
        if let AgentEvent::ApprovalRequest {
            request_id, title, ..
        } = e
        {
            asked += 1;
            assert_eq!(title, "Write probe file");
            s.approve(request_id.clone(), ApprovalDecision::Allow)
                .unwrap();
        }
    })
    .await;
    assert_eq!(asked, 1);
    assert_eq!(status(&events), TurnStatus::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { call_id, exit_code: Some(0), is_error: false, .. } if call_id == "write-probe"
    )));
    assert_eq!(text(&events), "Created the requested probe file.");
    let (expected, actual) = replay.pairs("permission.response").swap_remove(0);
    assert_eq!(actual["result"], expected["result"]);
    shutdown(session).await;
    replay.check_prompts();
    assert!(replay.unexpected().is_empty());
}

#[tokio::test]
async fn cancel_ends_interrupted() {
    let replay = Replay::new("turn_interrupt");
    let mut session = replay.start().await;
    session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    session.interrupt().unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Interrupted);
    let (expected, actual) = replay.pairs("session.cancel").swap_remove(0);
    assert_eq!(actual["params"], expected["params"]);
    shutdown(session).await;
    replay.check_prompts();
    assert!(replay.unexpected().is_empty());
}

#[tokio::test]
async fn steering_cancels_and_resends_as_one_turn() {
    let replay = Replay::new("message_steering");
    let mut session = replay.start().await;
    session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    // Wait for the partial answer, then steer.
    let mut events = Vec::new();
    loop {
        let event = next(&mut session).await;
        let partial = matches!(event, AgentEvent::TextDelta { .. });
        events.push(event);
        if partial {
            break;
        }
    }
    session.steer("steer-1", replay.prompt(1)).unwrap();
    events.extend(turn(&mut session, |_, _| {}).await);
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
            .count(),
        1,
        "the cancelled half does not end the app turn"
    );
    assert_eq!(text(&events), "stesteering fixture observed");
    assert!(replay.pairs("session.cancel").len() == 1);
    shutdown(session).await;
    replay.check_prompts();
    assert!(replay.ended());
    assert!(replay.unexpected().is_empty());
}

#[tokio::test]
async fn plan_updates() {
    let replay = Replay::new("todo_list");
    let mut session = replay.start().await;
    session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    let plans: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Plan { steps } => Some(steps.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0][0].status, PlanStatus::InProgress);
    assert_eq!(plans[0][2].text, "Report completion");
    assert!(plans[1].iter().all(|s| s.status == PlanStatus::Completed));
    let tools = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolResult { .. }))
        .count();
    assert_eq!(tools, 2);
    shutdown(session).await;
    replay.check_prompts();
}

#[tokio::test]
async fn prompt_error_fails_the_turn() {
    let replay = Replay::new("stop_background_work_after_failed_turn");
    let mut session = replay.start().await;
    session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Failed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Error { message } if message.contains("Internal error")
    )));
    shutdown(session).await;
    replay.check_prompts();
}

#[tokio::test]
async fn configured_model_is_selected() {
    // A model the session offers but does not use yet goes through
    // `session/set_model` (the recording does not have it, so it shows up
    // as Blongo's addition); the offered model already current does not.
    let current = Replay::new("simple");
    let mut session = current.start_with(Some("recorded-model-a")).await;
    session_started(&mut session).await;
    shutdown(session).await;
    assert!(current.unexpected().is_empty());

    let other = Replay::new("simple");
    let mut session = other.start_with(Some("recorded-model-b")).await;
    session_started(&mut session).await;
    shutdown(session).await;
    assert_eq!(other.unexpected(), vec!["session/set_model"]);
    let set = other
        .log()
        .into_iter()
        .find_map(|e| e.get("unexpected").cloned())
        .unwrap();
    assert_eq!(set["params"]["sessionId"], "acp-replay-session-1");
    assert_eq!(set["params"]["modelId"], "recorded-model-b");

    // Unknown models are not sent.
    let unknown = Replay::new("simple");
    let mut session = unknown.start_with(Some("not-offered")).await;
    session_started(&mut session).await;
    shutdown(session).await;
    assert!(unknown.unexpected().is_empty());
}
