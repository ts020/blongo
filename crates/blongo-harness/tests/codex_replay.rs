//! Conformance against t3code's recorded `codex app-server` sessions
//! (codex-cli 0.156.1, tests/fixtures/t3code/*.ndjson, see
//! THIRD_PARTY_NOTICES.md). `replay_codex.py` plays the recorded server side;
//! the harness under test drives it like a real Codex. The tests check the
//! normalized events and that the frames Blongo sends carry what the real
//! server needed (thread/turn ids, input text, approval decision,
//! `excludeTurns` on resume).

use std::path::PathBuf;
use std::time::Duration;

use blongo_harness::codex::{self, CodexOptions};
use blongo_harness::{ApprovalDecision, Session, SessionConfig};
use blongo_protocol::{AgentEvent, TurnStatus};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A recorded scenario plus the replay's comparison log.
struct Replay {
    transcript: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

impl Replay {
    fn new(scenario: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "blongo-replay-{scenario}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            transcript: fixtures().join(format!("t3code/{scenario}.ndjson")),
            log: dir.join("log.ndjson"),
            state: dir.join("state"),
        }
    }

    fn config(&self) -> SessionConfig {
        SessionConfig::new(std::env::temp_dir())
            .executable(fixtures().join("replay_codex.py"))
            .env("CODEX_REPLAY_TRANSCRIPT", &self.transcript)
            .env("CODEX_REPLAY_LOG", &self.log)
            .env("CODEX_REPLAY_STATE", &self.state)
    }

    async fn start(&self, resume: Option<&str>) -> Session {
        let options = CodexOptions {
            resume_thread_id: resume.map(str::to_owned),
            ..CodexOptions::default()
        };
        codex::start_with(self.config(), options).await.unwrap()
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// (recorded, actual) frames for one label, in order.
    fn pairs(&self, label: &str) -> Vec<(Value, Value)> {
        self.log()
            .into_iter()
            .filter(|e| e["label"] == label)
            .map(|e| (e["expected"].clone(), e["actual"].clone()))
            .collect()
    }

    fn unexpected_methods(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|e| e.get("unexpected"))
            .map(|f| f["method"].as_str().unwrap_or("<response>").to_owned())
            .collect()
    }

    fn segment_ended(&self, segment: u64) -> bool {
        self.log().iter().any(|e| e["end"] == segment)
    }

    /// The recorded prompt of the n-th turn/start.
    fn prompt(&self, n: usize) -> String {
        let text = std::fs::read_to_string(&self.transcript).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["type"] == "expect_outbound" && e["label"] == "turn/start")
            .nth(n)
            .and_then(|e| {
                e.pointer("/frame/params/input/0/text")?
                    .as_str()
                    .map(str::to_owned)
            })
            .expect("recorded turn/start")
    }
}

async fn next(session: &mut Session) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(20), session.next_event())
        .await
        .expect("timed out")
        .expect("session ended")
}

/// Events until the turn ends; `on_event` may act on each one.
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

async fn session_started(session: &mut Session) -> String {
    match next(session).await {
        AgentEvent::SessionStarted {
            provider_session_id,
        } => provider_session_id,
        other => panic!("expected SessionStarted, got {other:?}"),
    }
}

/// The recorded thread id; turn/start must address it and carry the prompt.
fn check_turn_start(replay: &Replay, n: usize, thread: &str) {
    let (expected, actual) = replay.pairs("turn/start").swap_remove(n);
    assert_eq!(actual["params"]["threadId"], expected["params"]["threadId"]);
    assert_eq!(actual["params"]["threadId"], thread);
    assert_eq!(
        actual["params"]["input"][0]["text"],
        expected["params"]["input"][0]["text"]
    );
    assert_eq!(actual["params"]["input"][0]["type"], "text");
}

async fn shutdown(session: Session) {
    tokio::time::timeout(Duration::from_secs(10), session.shutdown())
        .await
        .expect("shutdown timed out");
}

#[tokio::test]
async fn simple_text_turn() {
    let replay = Replay::new("simple");
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
    assert_eq!(thread, "01a0d5eb-f0c0-7481-a417-5e8871805cbc");
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(text(&events), "fixture simple ok");
    check_turn_start(&replay, 0, &thread);
    shutdown(session).await;
    assert!(replay.segment_ended(0), "{:#?}", replay.log());
    // Blongo's own sign-in check is the only frame the recording lacks.
    assert_eq!(replay.unexpected_methods(), vec!["account/read"]);
}

#[tokio::test]
async fn approval_round_trip_on_request() {
    let replay = Replay::new("tool_call_read_only_on_request");
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let mut approvals = Vec::new();
    let events = turn(&mut session, |s, e| {
        if let AgentEvent::ApprovalRequest {
            request_id, detail, ..
        } = e
        {
            approvals.push(detail.clone());
            s.approve(request_id.clone(), ApprovalDecision::Allow)
                .unwrap();
        }
    })
    .await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(approvals.len(), 1);
    assert!(approvals[0].contains("printf '%s' 'codex app-server approval fixture'"));
    assert!(approvals[0].contains("May I create or overwrite"));
    // Reasoning summary, commentary + final answer, the approved command.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ReasoningDelta { .. }))
    );
    let call = events.iter().find_map(|e| match e {
        AgentEvent::ToolCall { call_id, name, .. } => Some((call_id.clone(), name.clone())),
        _ => None,
    });
    assert_eq!(
        call,
        Some((
            "exec-02c3fc6e-7a54-4c15-afda-bc59a03b138a".into(),
            "shell".into()
        ))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult {
            is_error: false,
            exit_code: Some(0),
            ..
        }
    )));
    let reply = text(&events);
    assert!(
        reply.starts_with("I\u{2019}ll write the exact text"),
        "{reply}"
    );
    assert!(reply.contains("Created or overwrote"), "{reply}");
    check_turn_start(&replay, 0, &thread);
    assert_eq!(
        replay.pairs("turn/start")[0].1["params"]["approvalPolicy"],
        "on-request"
    );
    let (expected, actual) = replay
        .pairs("item/commandExecution/requestApproval")
        .swap_remove(0);
    assert_eq!(actual["id"], expected["id"]);
    assert_eq!(
        actual["result"], expected["result"],
        "approval response must match the recording"
    );
    shutdown(session).await;
    assert!(replay.segment_ended(0));
    assert_eq!(replay.unexpected_methods(), vec!["account/read"]);
}

/// Interrupt right after the prompt: the harness holds `turn/interrupt`
/// until it knows the turn id, which is where the recording expects it.
async fn interrupted_turn(scenario: &str) -> (Replay, Vec<AgentEvent>, String) {
    let replay = Replay::new(scenario);
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    session.interrupt().unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    check_turn_start(&replay, 0, &thread);
    let (expected, actual) = replay.pairs("turn/interrupt").swap_remove(0);
    assert_eq!(
        actual["params"], expected["params"],
        "interrupt must name the turn"
    );
    shutdown(session).await;
    assert!(replay.segment_ended(0), "{:#?}", replay.log());
    (replay, events, thread)
}

#[tokio::test]
async fn turn_interrupt() {
    let (replay, events, _) = interrupted_turn("turn_interrupt").await;
    assert_eq!(status(&events), TurnStatus::Interrupted);
    assert_eq!(text(&events), "");
    assert_eq!(replay.unexpected_methods(), vec!["account/read"]);
}

#[tokio::test]
async fn turn_interrupt_mid_tool() {
    let (replay, events, _) = interrupted_turn("turn_interrupt_mid_tool").await;
    assert_eq!(status(&events), TurnStatus::Interrupted);
    // The command started but never completed; the turn still closes.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { .. }))
    );
    // t3code also kills Codex's background terminal; Blongo does not.
    assert!(
        replay
            .log()
            .iter()
            .any(|e| e["skipped"] == "thread/backgroundTerminals/terminate")
    );
    assert_eq!(replay.unexpected_methods(), vec!["account/read"]);
}

#[tokio::test]
async fn provider_thread_resume_after_restart() {
    let replay = Replay::new("provider_thread_resume");
    // First process: a new Codex thread.
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(
        text(&events),
        "provider thread resume fixture first turn complete"
    );
    shutdown(session).await;
    assert!(replay.segment_ended(0));

    // Second process: resume the same thread without its history.
    let mut session = replay.start(Some(&thread)).await;
    assert_eq!(session_started(&mut session).await, thread);
    session.prompt(replay.prompt(1)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert!(text(&events).starts_with("provider thread resume fixture first turn complete\n"));
    let (expected, actual) = replay.pairs("thread/resume").swap_remove(0);
    assert_eq!(actual["params"]["threadId"], expected["params"]["threadId"]);
    assert_eq!(actual["params"]["excludeTurns"], true);
    assert_eq!(expected["params"]["excludeTurns"], true);
    check_turn_start(&replay, 1, &thread);
    shutdown(session).await;
    assert!(replay.segment_ended(1));
    assert_eq!(
        replay.unexpected_methods(),
        vec!["account/read", "account/read"]
    );
}
