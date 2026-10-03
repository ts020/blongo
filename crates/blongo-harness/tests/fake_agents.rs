//! End-to-end tests of the three harnesses against offline fake agents
//! (python3 scripts in tests/fixtures) speaking the real wire protocols.

use std::path::PathBuf;
use std::time::Duration;

use blongo_harness::{ApprovalDecision, Session, SessionConfig, acp, claude, codex};
use blongo_protocol::{AgentEvent, TurnStatus};

fn fixture(name: &str) -> PathBuf {
    runnable(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
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

fn config(script: &str) -> SessionConfig {
    SessionConfig::new(std::env::temp_dir()).executable(fixture(script))
}

/// Collect events until the turn ends, answering approvals with `decision`.
async fn run_turn(session: &mut Session, decision: ApprovalDecision) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), session.next_event())
            .await
            .expect("turn timed out")
            .expect("session ended mid-turn");
        if let AgentEvent::ApprovalRequest { request_id, .. } = &event {
            session.approve(request_id.clone(), decision).unwrap();
        }
        let done = matches!(event, AgentEvent::TurnCompleted { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

/// Start a "loop" turn, interrupt after a few deltas, return the end status.
async fn interrupt_loop(session: &mut Session) -> TurnStatus {
    session.prompt("loop").unwrap();
    let mut deltas = 0;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), session.next_event())
            .await
            .expect("timed out")
            .expect("ended");
        match event {
            AgentEvent::TextDelta { .. } => {
                deltas += 1;
                if deltas == 3 {
                    session.interrupt().unwrap();
                }
            }
            AgentEvent::TurnCompleted { status } => return status,
            _ => {}
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

fn tool_result(events: &[AgentEvent]) -> (bool, String) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                is_error, output, ..
            } => Some((*is_error, output.clone())),
            _ => None,
        })
        .expect("a tool result")
}

#[tokio::test(flavor = "current_thread")]
async fn claude_tool_approval_and_interrupt() {
    let mut session = claude::start(config("fake_claude.py")).await.unwrap();
    assert!(session.pid().is_some());
    session.prompt("list files").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::Allow).await;
    assert_eq!(
        events[0],
        AgentEvent::SessionStarted {
            provider_session_id: "fake-claude-session".into()
        }
    );
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "Let me list files.".into()
    }));
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::ApprovalRequest { title, detail, .. } if title == "Allow Bash?" && detail == "ls")));
    assert_eq!(tool_result(&events), (false, "Cargo.toml\nsrc".into()));
    assert_eq!(text(&events), "I'll run ls. Done.");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnCompleted {
            status: TurnStatus::Completed
        })
    );

    // Deny on the same session (the CLI is persistent across prompts).
    session.prompt("again").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::Deny).await;
    let (is_error, output) = tool_result(&events);
    assert!(is_error);
    assert!(output.contains("denied"), "{output}");

    assert_eq!(interrupt_loop(&mut session).await, TurnStatus::Interrupted);
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn codex_tool_approval_and_interrupt() {
    let mut session = codex::start(config("fake_codex.py").env("FAKE_CODEX_SIGNED_OUT", "1"))
        .await
        .unwrap();
    session.prompt("list files").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::AllowForSession).await;
    assert!(matches!(events[0], AgentEvent::AuthRequired { .. }));
    assert!(matches!(&events[1], AgentEvent::Models { models } if models.len() == 2));
    assert_eq!(
        events[2],
        AgentEvent::SessionStarted {
            provider_session_id: "thread-fake-1".into()
        }
    );
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "Planning".into()
    }));
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::ToolCall { call_id, name, .. } if call_id == "c1" && name == "shell")));
    assert_eq!(tool_result(&events), (false, "Cargo.toml\nsrc\n".into()));
    assert_eq!(text(&events), "Running ls. decision=acceptForSession");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnCompleted {
            status: TurnStatus::Completed
        })
    );

    session.prompt("again").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::Deny).await;
    assert!(tool_result(&events).0);
    assert!(text(&events).ends_with("decision=decline"));

    assert_eq!(interrupt_loop(&mut session).await, TurnStatus::Interrupted);

    // Interrupt while an approval is pending: answered with `cancel`.
    session.prompt("blocked").unwrap();
    loop {
        match session.next_event().await.unwrap() {
            AgentEvent::ApprovalRequest { .. } => session.interrupt().unwrap(),
            AgentEvent::TurnCompleted { status } => {
                assert_eq!(status, TurnStatus::Interrupted);
                break;
            }
            _ => {}
        }
    }
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_tool_permission_and_cancel() {
    let mut session = acp::start(config("fake_acp.py"), acp::antigravity())
        .await
        .unwrap();
    session.prompt("list files").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::AllowForSession).await;
    assert!(matches!(&events[0], AgentEvent::Models { models } if models.len() == 2));
    assert_eq!(
        events[1],
        AgentEvent::SessionStarted {
            provider_session_id: "sess-fake-1".into()
        }
    );
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::ApprovalRequest { title, detail, .. } if title == "Run command" && detail == "ls")));
    assert_eq!(tool_result(&events), (false, "Cargo.toml\nsrc\n".into()));
    assert_eq!(text(&events), "Listing files. option=allow-always");

    session.prompt("again").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::Deny).await;
    assert_eq!(tool_result(&events), (true, "denied".into()));
    assert!(text(&events).ends_with("option=reject"));

    assert_eq!(interrupt_loop(&mut session).await, TurnStatus::Interrupted);
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_signed_out_surfaces_oauth_url() {
    // stdout (as zeron/t3code describe it) and stderr (the real 1.2.1 and
    // 1.3.0 servers, whose URL is longer than a stderr tail line).
    for on_stderr in ["0", "1"] {
        let mut session = acp::start(
            config("fake_acp.py")
                .env("FAKE_ACP_SIGNED_OUT", "1")
                .env("FAKE_ACP_AUTH_STDERR", on_stderr),
            acp::antigravity(),
        )
        .await
        .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(20), session.next_event())
            .await
            .unwrap()
            .unwrap();
        match event {
            AgentEvent::AuthRequired { url: Some(url), .. } => {
                assert!(url.starts_with("https://accounts.google.com/"), "{url}");
                if on_stderr == "1" {
                    assert!(url.ends_with(&"x".repeat(500)), "URL cut short: {url}");
                }
            }
            other => panic!("expected AuthRequired (stderr={on_stderr}), got {other:?}"),
        }
        // The session ends (child reaped) instead of hanging on the browser.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), session.next_event())
                .await
                .unwrap(),
            None
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_executable_is_an_error() {
    let err = codex::start(SessionConfig::new(".").executable("/nonexistent/codex"))
        .await
        .err()
        .expect("spawn fails");
    assert!(err.to_string().contains("not found"), "{err}");
}

#[tokio::test(flavor = "current_thread")]
async fn crash_mid_turn_reports_failure() {
    // `true` exits immediately: the turn must still end, as Failed.
    // (macOS has no /bin/true; Windows has no `true` at all.)
    #[cfg(unix)]
    let exits_at_once = PathBuf::from("/usr/bin/true");
    #[cfg(windows)]
    let exits_at_once = {
        let path = std::env::temp_dir().join(format!("blongo-true-{}.cmd", std::process::id()));
        std::fs::write(&path, "@exit /b 0\r\n").unwrap();
        path
    };
    let mut session = claude::start(SessionConfig::new(".").executable(exits_at_once))
        .await
        .unwrap();
    session.prompt("hi").unwrap();
    let mut saw_error = false;
    while let Some(event) = session.next_event().await {
        match event {
            AgentEvent::Error { .. } => saw_error = true,
            AgentEvent::TurnCompleted { status } => {
                assert_eq!(status, TurnStatus::Failed);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_error);
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_resume_loads_without_replaying_history() {
    let agent = acp::AcpAgent {
        resume_session: Some("sess-earlier".into()),
        extra_paths: Vec::new(),
        ..acp::antigravity()
    };
    let mut config = config("fake_acp.py");
    config.model = Some("fake-model-b".into());
    let mut session = acp::start(config, agent).await.unwrap();
    session.prompt("model?").unwrap();
    let events = run_turn(&mut session, ApprovalDecision::Allow).await;
    assert!(events.contains(&AgentEvent::SessionStarted {
        provider_session_id: "sess-earlier".into()
    }));
    // The history chunk `session/load` replays is not a new message, and
    // the configured model was selected on the loaded session.
    assert_eq!(text(&events), "model=fake-model-b");
    session.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_login_waits_for_the_browser() {
    for on_stderr in ["0", "1"] {
        let dir =
            std::env::temp_dir().join(format!("blongo-login-{}-{on_stderr}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let done = dir.join("done");
        let _ = std::fs::remove_file(&done);
        let (url_tx, mut url_rx) = tokio::sync::mpsc::channel(4);
        let config = config("fake_acp.py")
            .env("FAKE_ACP_SIGNED_OUT", "1")
            .env("FAKE_ACP_AUTH_STDERR", on_stderr)
            .env("FAKE_ACP_LOGIN_FILE", &done);
        let login = tokio::spawn(acp::login(
            config,
            acp::antigravity(),
            url_tx,
            Duration::from_secs(20),
        ));
        let url = tokio::time::timeout(Duration::from_secs(20), url_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(url.starts_with("https://accounts.google.com/"), "{url}");
        assert!(!login.is_finished(), "login must wait for the browser");
        // The user finishes signing in.
        std::fs::write(&done, "").unwrap();
        tokio::time::timeout(Duration::from_secs(20), login)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_login_times_out() {
    let (url_tx, _url_rx) = tokio::sync::mpsc::channel(4);
    let err = acp::login(
        config("fake_acp.py").env("FAKE_ACP_SIGNED_OUT", "1"),
        acp::antigravity(),
        url_tx,
        Duration::from_millis(500),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
}

/// A steer that arrives after the turn ended comes back as
/// `SteerNotDelivered`; the harness never runs it as a turn of its own.
#[tokio::test(flavor = "current_thread")]
async fn steer_after_the_turn_ended_is_returned_not_run() {
    let sessions = [
        claude::start(config("fake_claude.py")).await.unwrap(),
        codex::start(config("fake_codex.py")).await.unwrap(),
        acp::start(config("fake_acp.py"), acp::antigravity())
            .await
            .unwrap(),
    ];
    for mut session in sessions {
        session.prompt("echo: one").unwrap();
        run_turn(&mut session, ApprovalDecision::Allow).await;
        session.steer("late-1", "echo: late").unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), session.next_event())
            .await
            .expect("an answer to the steer")
            .expect("session alive");
        assert_eq!(
            event,
            AgentEvent::SteerNotDelivered {
                id: "late-1".into()
            }
        );
        // Nothing else: no hidden turn started.
        assert!(
            tokio::time::timeout(Duration::from_millis(700), session.next_event())
                .await
                .is_err(),
            "the late steer started a turn"
        );
        session.shutdown().await;
    }
}

/// A steer sent while an interrupt is in flight is not delivered either.
#[tokio::test(flavor = "current_thread")]
async fn steer_during_an_interrupt_is_returned() {
    let sessions = [
        claude::start(config("fake_claude.py")).await.unwrap(),
        codex::start(config("fake_codex.py")).await.unwrap(),
        acp::start(config("fake_acp.py"), acp::antigravity())
            .await
            .unwrap(),
    ];
    for mut session in sessions {
        session.prompt("loop").unwrap();
        let mut saw_not_delivered = false;
        let mut deltas = 0;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), session.next_event())
                .await
                .expect("timed out")
                .expect("ended");
            match event {
                AgentEvent::TextDelta { .. } => {
                    deltas += 1;
                    if deltas == 3 {
                        session.interrupt().unwrap();
                        session.steer("during", "echo: too late").unwrap();
                    }
                }
                AgentEvent::SteerNotDelivered { id } => {
                    assert_eq!(id, "during");
                    saw_not_delivered = true;
                }
                AgentEvent::TurnCompleted { status } => {
                    assert_eq!(status, TurnStatus::Interrupted);
                    break;
                }
                _ => {}
            }
        }
        if !saw_not_delivered {
            let event = tokio::time::timeout(Duration::from_secs(5), session.next_event())
                .await
                .expect("steer answer")
                .unwrap();
            assert_eq!(
                event,
                AgentEvent::SteerNotDelivered {
                    id: "during".into()
                }
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(700), session.next_event())
                .await
                .is_err(),
            "the steer started a turn"
        );
        session.shutdown().await;
    }
}
