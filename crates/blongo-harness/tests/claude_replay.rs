//! Conformance against t3code's recorded Claude sessions (claude 2.1.111
//! through claude-agent-sdk 0.2.111, tests/fixtures/t3code/claude/*.ndjson,
//! see THIRD_PARTY_NOTICES.md). `replay_claude.py` plays the CLI side of the
//! stream-json protocol from those recordings; the harness under test
//! drives it like the real `claude`. The tests check the normalized events
//! and that what Blongo sends matches what the SDK sent: prompt text,
//! steering priority, permission decisions with `toolUseID`, the interrupt,
//! and the launch flags for resume-at (rollback) and native fork.

use std::path::PathBuf;
use std::time::Duration;

use blongo_harness::claude::{self, ClaudeOptions};
use blongo_harness::{ApprovalDecision, Session, SessionConfig};
use blongo_protocol::{AgentEvent, TurnStatus};
use serde_json::Value;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
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
            "blongo-claude-replay-{scenario}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            transcript: fixtures().join(format!("t3code/claude/{scenario}.ndjson")),
            log: dir.join("log.ndjson"),
            state: dir.join("state"),
        }
    }

    async fn start(&self, options: ClaudeOptions) -> Session {
        let config = SessionConfig::new(std::env::temp_dir())
            .executable(fixtures().join("replay_claude.py"))
            .env("CLAUDE_REPLAY_TRANSCRIPT", &self.transcript)
            .env("CLAUDE_REPLAY_LOG", &self.log)
            .env("CLAUDE_REPLAY_STATE", &self.state);
        claude::start_with(config, options).await.unwrap()
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

    /// Client frames the recording has no place for, by control subtype or
    /// frame type.
    fn unexpected(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|e| e.get("unexpected"))
            .map(|f| {
                f.pointer("/request/subtype")
                    .or_else(|| f.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect()
    }

    fn argv(&self, segment: u64) -> Vec<String> {
        self.log()
            .iter()
            .find(|e| e["segment"] == segment)
            .and_then(|e| e["argv"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect()
    }

    fn ended(&self, segment: u64) -> bool {
        self.log().iter().any(|e| e["end"] == segment)
    }

    /// Text of the recording's n-th prompt.offer.
    fn prompt(&self, n: usize) -> String {
        std::fs::read_to_string(&self.transcript)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["type"] == "expect_outbound" && e["frame"]["type"] == "prompt.offer")
            .nth(n)
            .and_then(|e| {
                e.pointer("/frame/message/message/content")?
                    .as_str()
                    .map(str::to_owned)
            })
            .expect("recorded prompt")
    }

    /// Blongo's prompt lines must carry the recorded user message.
    fn check_prompts(&self) {
        let pairs: Vec<_> = self
            .log()
            .into_iter()
            .filter(|e| e["expected"]["type"] == "prompt.offer")
            .collect();
        assert!(!pairs.is_empty());
        for pair in pairs {
            let expected = &pair["expected"]["message"];
            let actual = &pair["actual"];
            assert_eq!(actual["type"], "user");
            assert_eq!(actual["message"]["role"], "user");
            assert_eq!(actual["message"]["content"], expected["message"]["content"]);
            assert_eq!(actual["parent_tool_use_id"], Value::Null);
            assert_eq!(actual.get("priority"), expected.get("priority"));
        }
    }
}

async fn next(session: &mut Session) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(20), session.next_event())
        .await
        .expect("timed out")
        .expect("session ended")
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

fn session_id(events: &[AgentEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentEvent::SessionStarted {
            provider_session_id,
        } => Some(provider_session_id.clone()),
        _ => None,
    })
}

fn turn_id(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ProviderTurnId { id } => Some(id.clone()),
            _ => None,
        })
        .expect("provider turn id")
}

fn models(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Models { models } => Some(models.iter().map(|m| m.id.clone())),
            _ => None,
        })
        .flatten()
        .collect()
}

async fn shutdown(session: Session) {
    tokio::time::timeout(Duration::from_secs(10), session.shutdown())
        .await
        .expect("shutdown timed out");
}

fn bypass() -> ClaudeOptions {
    ClaudeOptions {
        permission_mode: "bypassPermissions".into(),
        ..ClaudeOptions::default()
    }
}

/// Blongo's `initialize` handshake is the only frame the SDK recording
/// does not show (the SDK sends it too, below the recorded API).
fn handshake_only(replay: &Replay, processes: usize) {
    assert_eq!(replay.unexpected(), vec!["initialize"; processes]);
}

#[tokio::test]
async fn simple_text_turn() {
    let replay = Replay::new("simple");
    let mut session = replay.start(bypass()).await;
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(text(&events), "fixture simple ok");
    assert_eq!(
        session_id(&events).as_deref(),
        Some("77171d01-fb4f-4dff-a961-d9dd334be93d")
    );
    assert_eq!(models(&events), vec!["recorded-model"]);
    assert_eq!(turn_id(&events), "502cdf40-1af1-428c-87cb-a07cfc99dafd");
    shutdown(session).await;
    replay.check_prompts();
    assert!(replay.ended(0), "{:#?}", replay.log());
    handshake_only(&replay, 1);
    let argv = replay.argv(0);
    assert!(
        argv.windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"])
    );
    assert!(!argv.contains(&"--resume".to_owned()));
}

#[tokio::test]
async fn approval_allowed_carries_tool_use_id() {
    let replay = Replay::new("tool_call_read_only_on_request");
    let mut session = replay.start(ClaudeOptions::default()).await;
    session.prompt(replay.prompt(0)).unwrap();
    let mut asked = Vec::new();
    let events = turn(&mut session, |s, e| {
        if let AgentEvent::ApprovalRequest {
            request_id, detail, ..
        } = e
        {
            asked.push(detail.clone());
            s.approve(request_id.clone(), ApprovalDecision::Allow)
                .unwrap();
        }
    })
    .await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(asked.len(), 1);
    assert!(asked[0].starts_with("printf 'codex app-server approval fixture'"));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCall { name, .. } if name == "Bash"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolResult { is_error: false, output, .. } if output == "(Bash completed with no output)"
    )));
    assert!(text(&events).starts_with("Done."));
    let (expected, actual) = replay.pairs("permission.response:Bash").swap_remove(0);
    let response = &actual["response"]["response"];
    assert_eq!(actual["response"]["subtype"], "success");
    assert_eq!(response["behavior"], expected["result"]["behavior"]);
    assert_eq!(response["updatedInput"], expected["result"]["updatedInput"]);
    assert_eq!(response["toolUseID"], expected["result"]["toolUseID"]);
    shutdown(session).await;
    replay.check_prompts();
    assert!(
        !replay
            .log()
            .iter()
            .any(|e| e.get("wrong_request_id").is_some())
    );
    handshake_only(&replay, 1);
}

#[tokio::test]
async fn approval_denied_write() {
    let replay = Replay::new("tool_call_denied_write");
    let mut session = replay.start(ClaudeOptions::default()).await;
    session.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut session, |s, e| {
        if let AgentEvent::ApprovalRequest { request_id, .. } = e {
            s.approve(request_id.clone(), ApprovalDecision::Deny)
                .unwrap();
        }
    })
    .await;
    assert_eq!(status(&events), TurnStatus::Completed);
    // Streamed once (deltas), not repeated by the assistant frame.
    assert_eq!(text(&events), "write permission denied");
    let tool_input = events.iter().find_map(|e| match e {
        AgentEvent::ToolCall { name, input, .. } if name == "Write" => Some(input.clone()),
        _ => None,
    });
    assert_eq!(
        tool_input.unwrap()["content"],
        "approval fixture",
        "the full tool input comes from the assistant frame"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { is_error: true, .. }))
    );
    let (expected, actual) = replay.pairs("permission.response:Write").swap_remove(0);
    let response = &actual["response"]["response"];
    assert_eq!(response["behavior"], "deny");
    assert_eq!(response["toolUseID"], expected["result"]["toolUseID"]);
    shutdown(session).await;
    replay.check_prompts();
    handshake_only(&replay, 1);
}

async fn interrupted(scenario: &str) -> (Replay, Vec<AgentEvent>) {
    let replay = Replay::new(scenario);
    let mut session = replay.start(bypass()).await;
    session.prompt(replay.prompt(0)).unwrap();
    session.interrupt().unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Interrupted);
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    shutdown(session).await;
    replay.check_prompts();
    let (_, actual) = replay.pairs("query.interrupt:1").swap_remove(0);
    assert_eq!(actual["request"]["subtype"], "interrupt");
    handshake_only(&replay, 1);
    (replay, events)
}

#[tokio::test]
async fn turn_interrupt() {
    interrupted("turn_interrupt").await;
}

#[tokio::test]
async fn turn_interrupt_mid_tool() {
    let (_, events) = interrupted("turn_interrupt_mid_tool").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }))
    );
}

#[tokio::test]
async fn message_steering_is_one_turn() {
    let replay = Replay::new("message_steering");
    let mut session = replay.start(bypass()).await;
    session.prompt(replay.prompt(0)).unwrap();
    session.steer("steer-1", replay.prompt(1)).unwrap();
    let events = turn(&mut session, |_, _| {}).await;
    // The CLI's error_during_execution for the cut-short part is not a
    // failure: the steered turn completes once.
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(text(&events), "steering fixture observed");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert_eq!(turn_id(&events), "2faa9bee-1c72-4b8f-ab04-9796a7ecf6a2");
    shutdown(session).await;
    replay.check_prompts();
    let (_, steer) = replay.pairs("prompt.offer:2").swap_remove(0);
    assert_eq!(steer["priority"], "now");
    handshake_only(&replay, 1);
}

#[tokio::test]
async fn queued_turns_run_in_order() {
    let replay = Replay::new("queued_turn");
    let mut session = replay.start(bypass()).await;
    session.prompt(replay.prompt(0)).unwrap();
    let first = turn(&mut session, |_, _| {}).await;
    assert_eq!(text(&first), "first fixture turn complete");
    session.prompt(replay.prompt(1)).unwrap();
    let second = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&second), TurnStatus::Completed);
    assert_eq!(text(&second), "second fixture turn complete");
    // One session, announced once.
    assert!(session_id(&second).is_none());
    shutdown(session).await;
    replay.check_prompts();
    handshake_only(&replay, 1);
}

#[tokio::test]
async fn rollback_resumes_at_the_kept_turn() {
    let replay = Replay::new("thread_rollback");
    let mut session = replay.start(bypass()).await;
    session.prompt(replay.prompt(0)).unwrap();
    let first = turn(&mut session, |_, _| {}).await;
    let session_id = session_id(&first).unwrap();
    let kept = turn_id(&first);
    session.prompt(replay.prompt(1)).unwrap();
    let second = turn(&mut session, |_, _| {}).await;
    assert_eq!(text(&second), "rollback fixture second turn complete");
    shutdown(session).await;

    // Roll back the second turn: a new process resumes at the first.
    let mut session = replay
        .start(ClaudeOptions {
            resume: Some(session_id.clone()),
            resume_session_at: Some(kept.clone()),
            ..bypass()
        })
        .await;
    session.prompt(replay.prompt(2)).unwrap();
    let third = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&third), TurnStatus::Completed);
    assert!(text(&third).starts_with("Here is the conversation verbatim"));
    shutdown(session).await;
    replay.check_prompts();
    let (expected, _) = replay.pairs("query.open:resume_at_cursor").swap_remove(0);
    assert_eq!(expected["options"]["resume"], session_id.as_str());
    assert_eq!(expected["options"]["resumeSessionAt"], kept.as_str());
    let argv = replay.argv(1);
    assert!(
        argv.windows(2)
            .any(|w| w == ["--resume", session_id.as_str()])
    );
    assert!(argv.contains(&format!("--resume-session-at={kept}")));
    assert!(!argv.contains(&"--fork-session".to_owned()));
    assert!(replay.ended(1));
    handshake_only(&replay, 2);
}

#[tokio::test]
async fn native_fork_branches_the_session() {
    let replay = Replay::new("thread_fork_native");
    let mut source = replay.start(bypass()).await;
    source.prompt(replay.prompt(0)).unwrap();
    let events = turn(&mut source, |_, _| {}).await;
    let source_session = session_id(&events).unwrap();
    let up_to = turn_id(&events);
    shutdown(source).await;

    let mut fork = replay
        .start(ClaudeOptions {
            resume: Some(source_session.clone()),
            fork_session: true,
            resume_session_at: Some(up_to.clone()),
            ..bypass()
        })
        .await;
    fork.prompt(replay.prompt(1)).unwrap();
    let events = turn(&mut fork, |_, _| {}).await;
    assert_eq!(status(&events), TurnStatus::Completed);
    assert_eq!(text(&events), "fork native ok");
    // The fork is a new provider session.
    assert_eq!(
        session_id(&events).as_deref(),
        Some("7e7ad6ec-db4a-4510-85ff-59df35cf7fad")
    );
    shutdown(fork).await;
    replay.check_prompts();
    let (expected, _) = replay.pairs("session.fork").swap_remove(0);
    assert_eq!(expected["sessionId"], source_session.as_str());
    assert_eq!(expected["options"]["upToMessageId"], up_to.as_str());
    let argv = replay.argv(1);
    assert!(
        argv.windows(2)
            .any(|w| w == ["--resume", source_session.as_str()])
    );
    assert!(argv.contains(&"--fork-session".to_owned()));
    assert!(argv.contains(&format!("--resume-session-at={up_to}")));
    handshake_only(&replay, 2);
}

#[tokio::test]
async fn result_is_error_fails_the_turn_then_recovers() {
    let replay = Replay::new("claude_result_is_error");
    let mut session = replay.start(bypass()).await;
    session.prompt(replay.prompt(0)).unwrap();
    let first = turn(&mut session, |_, _| {}).await;
    // `subtype: success` with `is_error: true` is a failed turn.
    assert_eq!(status(&first), TurnStatus::Failed);
    assert!(first.iter().any(|e| matches!(
        e,
        AgentEvent::AuthRequired { message, .. } if message.contains("401")
    )));
    session.prompt(replay.prompt(1)).unwrap();
    let second = turn(&mut session, |_, _| {}).await;
    assert_eq!(status(&second), TurnStatus::Completed);
    assert_eq!(text(&second), "claude result is_error fixture recovered");
    shutdown(session).await;
    replay.check_prompts();
    handshake_only(&replay, 1);
}
