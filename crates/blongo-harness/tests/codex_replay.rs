//! Conformance against t3code's recorded `codex app-server` sessions
//! (codex-cli 0.156.1, tests/fixtures/t3code/*.ndjson, see
//! THIRD_PARTY_NOTICES.md). `replay_codex.py` plays the recorded server side;
//! the harness under test drives it like a real Codex. The tests check the
//! normalized events and that the frames Blongo sends carry what the real
//! server needed (thread/turn ids, input text, approval decision,
//! `excludeTurns` on resume, `expectedTurnId` on steer, `lastTurnId` on
//! fork, `beforeTurnId` on revert).
//!
//! Frames Blongo sends that t3code did not (`account/read`, `model/list`)
//! get canned answers from the replay and are checked by
//! [`BLONGO_ONLY`].

use std::path::PathBuf;
use std::time::Duration;

use blongo_harness::codex::{self, CodexOptions};
use blongo_harness::{ApprovalDecision, Session, SessionConfig};
use blongo_protocol::{AgentEvent, PlanStatus, TurnStatus};
use serde_json::Value;

/// Setup requests Blongo adds per process: its sign-in check and model
/// discovery.
const BLONGO_ONLY: [&str; 2] = ["account/read", "model/list"];

fn blongo_only(processes: usize) -> Vec<String> {
    (0..processes)
        .flat_map(|_| BLONGO_ONLY.map(str::to_owned))
        .collect()
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The fake agents are Python scripts that run through their `#!` line.
/// Windows has no shebangs, so there they run through a `.cmd` wrapper
/// that calls `python` in UTF-8 mode (the fixtures are UTF-8; Windows'
/// default code page would garble them).
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
            std::fs::write(
                &tmp,
                format!(
                    "@set PYTHONUTF8=1\r\n@python \"{}\" %*\r\n",
                    script.display()
                ),
            )
            .unwrap();
            if std::fs::rename(&tmp, &shim).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        shim
    }
    #[cfg(not(windows))]
    script
}

/// A recorded scenario plus the replay's comparison log.
struct Replay {
    transcript: PathBuf,
    log: PathBuf,
    state: PathBuf,
    split: Option<String>,
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
            split: None,
        }
    }

    fn split_at(mut self, label: &str) -> Self {
        self.split = Some(label.to_owned());
        self
    }

    fn config(&self) -> SessionConfig {
        let config = SessionConfig::new(std::env::temp_dir());
        let config = match &self.split {
            Some(label) => config.env("CODEX_REPLAY_SPLIT_AT", label),
            None => config,
        };
        config
            .executable(runnable(fixtures().join("replay_codex.py")))
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

    async fn start_with(&self, options: CodexOptions) -> Session {
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
        self.recorded_text(|label| label.starts_with("turn/start"), n)
    }

    /// The input text of the n-th expected frame whose label passes.
    fn recorded_text(&self, label: impl Fn(&str) -> bool, n: usize) -> String {
        let text = std::fs::read_to_string(&self.transcript).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["type"] == "expect_outbound" && label(e["label"].as_str().unwrap_or("")))
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

/// Skips setup events (`Models`) up to `SessionStarted`.
async fn session_started(session: &mut Session) -> String {
    loop {
        match next(session).await {
            AgentEvent::SessionStarted {
                provider_session_id,
            } => return provider_session_id,
            AgentEvent::Models { models } => {
                // The replay's canned page: hidden models are dropped.
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].id, "recorded-model");
                assert_eq!(models[0].label, "Recorded Model");
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
    }
}

fn provider_turn_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ProviderTurnId { id } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// The recorded thread id; turn/start must address it and carry the prompt.
fn check_turn_start(replay: &Replay, n: usize, thread: &str) {
    check_turn_start_labeled(replay, "turn/start", n, thread);
}

fn check_turn_start_labeled(replay: &Replay, label: &str, n: usize, thread: &str) {
    let (expected, actual) = replay.pairs(label).swap_remove(n);
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
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
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
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
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
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
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
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
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
    assert_eq!(replay.unexpected_methods(), blongo_only(2));
}

#[tokio::test]
async fn message_steering_uses_turn_steer() {
    let replay = Replay::new("message_steering");
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    // Sent before the turn id is known: the harness holds it until then.
    let steer = replay.recorded_text(|l| l == "turn/steer", 0);
    session.steer("steer-1", steer.clone()).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert!(
        text(&events).ends_with("steering fixture observed"),
        "{}",
        text(&events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
            .count(),
        1
    );
    check_turn_start(&replay, 0, &thread);
    let (expected, actual) = replay.pairs("turn/steer").swap_remove(0);
    assert_eq!(actual["params"]["threadId"], expected["params"]["threadId"]);
    assert_eq!(
        actual["params"]["expectedTurnId"],
        expected["params"]["expectedTurnId"]
    );
    assert_eq!(
        provider_turn_ids(&events),
        vec![
            expected["params"]["expectedTurnId"]
                .as_str()
                .unwrap()
                .to_owned()
        ]
    );
    assert_eq!(actual["params"]["input"][0]["text"], steer.as_str());
    shutdown(session).await;
    assert!(replay.segment_ended(0), "{:#?}", replay.log());
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
}

#[tokio::test]
async fn thread_rollback_reverts_live() {
    let replay = Replay::new("thread_rollback");
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
    session.prompt(replay.prompt(0)).unwrap();
    let first = turn(&mut session, |_, _| {}).await;
    assert_eq!(text(&first), "rollback fixture first turn complete");
    session.prompt(replay.prompt(1)).unwrap();
    let second = turn(&mut session, |_, _| {}).await;
    assert_eq!(text(&second), "rollback fixture second turn complete");
    // Roll back the second turn by its provider turn id, then continue.
    let second_turn = provider_turn_ids(&second).swap_remove(0);
    session.rewind(second_turn.clone()).unwrap();
    session.prompt(replay.prompt(2)).unwrap();
    let third = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&third), TurnStatus::Completed);
    let (expected, actual) = replay.pairs("thread/revert").swap_remove(0);
    assert_eq!(actual["params"], expected["params"]);
    assert_eq!(actual["params"]["beforeTurnId"], second_turn.as_str());
    check_turn_start(&replay, 2, &thread);
    shutdown(session).await;
    assert!(replay.segment_ended(0), "{:#?}", replay.log());
    let skipped: Vec<_> = replay
        .log()
        .iter()
        .filter_map(|e| e["skipped"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(skipped, vec!["thread/read", "thread/turns/list"]);
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
}

#[tokio::test]
async fn thread_fork_native_in_its_own_process() {
    // t3code forks inside one app-server; Blongo's fork thread starts its
    // own process, so the replay splits the recording at `thread/fork`.
    let replay = Replay::new("thread_fork_native").split_at("thread/fork");
    let mut source = replay.start(None).await;
    let source_thread = session_started(&mut source).await;
    assert_eq!(source_thread, "native-source-thread");
    source.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut source, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    let source_turn = provider_turn_ids(&events).swap_remove(0);
    assert_eq!(source_turn, "native-source-turn");
    check_turn_start_labeled(&replay, "turn/start/source", 0, &source_thread);
    shutdown(source).await;
    assert!(replay.segment_ended(0), "{:#?}", replay.log());

    let mut fork = replay
        .start_with(CodexOptions {
            fork: Some((source_thread.clone(), Some(source_turn.clone()))),
            ..CodexOptions::default()
        })
        .await;
    let fork_thread = session_started(&mut fork).await;
    assert_eq!(fork_thread, "native-fork-thread");
    fork.prompt(replay.prompt(1)).unwrap();
    let events = turn(&mut fork, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    let (expected, actual) = replay.pairs("thread/fork").swap_remove(0);
    assert_eq!(actual["params"]["threadId"], expected["params"]["threadId"]);
    assert_eq!(
        actual["params"]["lastTurnId"],
        expected["params"]["lastTurnId"]
    );
    check_turn_start_labeled(&replay, "turn/start/fork", 0, &fork_thread);
    shutdown(fork).await;
    assert!(replay.segment_ended(1), "{:#?}", replay.log());
    // The second process's handshake is Blongo's addition too.
    let mut unexpected = blongo_only(1);
    unexpected.extend(["initialize", "initialized"].map(str::to_owned));
    unexpected.extend(blongo_only(1));
    assert_eq!(replay.unexpected_methods(), unexpected);
}

#[tokio::test]
async fn todo_list_becomes_plan_events() {
    let replay = Replay::new("todo_list");
    let mut session = replay.start(None).await;
    let thread = session_started(&mut session).await;
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
    assert_eq!(plans[0].len(), 3);
    assert_eq!(plans[0][0].text, "Inspect package.json");
    assert_eq!(plans[0][0].status, PlanStatus::InProgress);
    assert_eq!(plans[0][1].status, PlanStatus::Pending);
    assert!(plans[1].iter().all(|s| s.status == PlanStatus::Completed));
    check_turn_start(&replay, 0, &thread);
    shutdown(session).await;
    assert!(replay.segment_ended(0), "{:#?}", replay.log());
    assert_eq!(replay.unexpected_methods(), blongo_only(1));
}
