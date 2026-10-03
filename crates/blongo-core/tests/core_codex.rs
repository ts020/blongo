//! The orchestrator end to end against the offline fake `codex app-server`
//! (crates/blongo-harness/tests/fixtures/fake_codex.py): full turns with
//! approval, deny, interrupt, idempotency, restart restore and crash
//! recovery.

mod common;

use common::*;

fn kinds(snapshot: &ThreadSnapshot) -> Vec<&'static str> {
    snapshot.items.iter().map(|i| i.kind.tag()).collect()
}

fn texts(snapshot: &ThreadSnapshot, tag: &str) -> Vec<String> {
    snapshot
        .items
        .iter()
        .filter(|i| i.kind.tag() == tag)
        .map(|i| i.text.to_string())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn full_turns_approval_deny_interrupt_and_restore() {
    let dir = temp_dir("full");
    let (mut core, shell) = TestCore::start(&dir);
    assert!(shell.projects.is_empty());
    let (_, thread_id) = core.project_and_thread(&dir).await;

    // Turn 1: approval → approve.
    let first = core.send(thread_id, "list files");
    let mut streamed = String::new();
    let approval = core
        .until(|e| match e {
            CoreEvent::TextDelta { chunk, .. } => {
                streamed.push_str(chunk);
                None
            }
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ItemAdded { item }
                    if matches!(item.kind, ItemKind::ApprovalRequest { .. }) =>
                {
                    Some(item.clone())
                }
                _ => None,
            },
            _ => None,
        })
        .await;
    assert_eq!(streamed, "PlanningRunning ls.");
    core.dispatch(Command::RuntimeRequestRespond {
        thread_id,
        item_id: approval.id,
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(core.run_finished().await, RunStatus::Completed);

    let snap = core.snapshot(thread_id).await;
    assert_eq!(
        kinds(&snap),
        [
            "user_message",
            "reasoning",
            "assistant_message",
            "command_execution",
            "approval_request",
            "assistant_message"
        ]
    );
    assert_eq!(texts(&snap, "user_message"), ["list files"]);
    assert_eq!(texts(&snap, "reasoning"), ["Planning"]);
    assert_eq!(
        texts(&snap, "assistant_message"),
        ["Running ls.", " decision=accept"]
    );
    for item in &snap.items {
        match &item.kind {
            ItemKind::AssistantMessage { streaming } | ItemKind::Reasoning { streaming } => {
                assert!(!streaming)
            }
            ItemKind::CommandExecution {
                command,
                status,
                output,
                ..
            } => {
                assert_eq!(command, "ls");
                assert_eq!(*status, ToolStatus::Completed);
                assert_eq!(output, "Cargo.toml\nsrc\n");
            }
            ItemKind::ApprovalRequest { state, detail, .. } => {
                assert_eq!(*state, ApprovalState::Approved);
                assert!(detail.starts_with("ls"));
            }
            _ => {}
        }
    }
    let ordinals: Vec<u32> = snap.items.iter().map(|i| i.ordinal).collect();
    assert!(ordinals.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(snap.runs.len(), 1);
    assert_eq!(snap.runs[0].status, RunStatus::Completed);

    // Replaying the same command is a no-op.
    core.handle().dispatch(first.clone());
    core.until(|e| match e {
        CoreEvent::CommandDuplicate { command_id } if *command_id == first.command_id => Some(()),
        CoreEvent::Event(ev) => panic!("duplicate produced an event: {ev:?}"),
        _ => None,
    })
    .await;

    // Turn 2: deny.
    core.send(thread_id, "again");
    let approval = core
        .added_item(|i| matches!(i.kind, ItemKind::ApprovalRequest { .. }))
        .await;
    core.dispatch(Command::RuntimeRequestRespond {
        thread_id,
        item_id: approval.id,
        decision: ApprovalDecision::Deny,
    });
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let snap = core.snapshot(thread_id).await;
    assert_eq!(
        texts(&snap, "assistant_message").last().unwrap(),
        " decision=decline"
    );
    let denied = snap
        .items
        .iter()
        .filter(|i| {
            matches!(
                i.kind,
                ItemKind::ApprovalRequest {
                    state: ApprovalState::Denied,
                    ..
                }
            )
        })
        .count();
    assert_eq!(denied, 1);

    // Turn 3: a message sent meanwhile queues (and is cancelled again),
    // then interrupt a streaming turn.
    core.send(thread_id, "loop");
    core.until(|e| match e {
        CoreEvent::TextDelta { chunk, .. } if chunk.starts_with("tick 3") => Some(()),
        _ => None,
    })
    .await;
    let queued_run = RunId::new();
    core.dispatch(Command::MessageDispatch {
        thread_id,
        message_id: ItemId::new(),
        run_id: queued_run,
        text: "too soon".into(),
        delivery: Delivery::Queue,
    });
    core.until(|e| match e {
        CoreEvent::Event(ev) => match &ev.kind {
            EventKind::RunCreated { run } if run.id == queued_run => {
                assert_eq!(run.status, RunStatus::Queued);
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    core.dispatch(Command::RunCancel {
        thread_id,
        run_id: queued_run,
    });
    core.until(|e| match e {
        CoreEvent::Event(ev) => match &ev.kind {
            EventKind::RunStatusChanged { run_id, status, .. } if *run_id == queued_run => {
                assert_eq!(*status, RunStatus::Cancelled);
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    core.dispatch(Command::RunInterrupt { thread_id });
    assert_eq!(core.run_finished().await, RunStatus::Interrupted);
    let snap = core.snapshot(thread_id).await;
    let last = snap.items.last().unwrap();
    assert!(
        matches!(&last.kind, ItemKind::SystemNotice { message } if message.contains("interrupted"))
    );
    let ticks = texts(&snap, "assistant_message").last().unwrap().clone();
    assert!(ticks.starts_with("tick 1 tick 2 tick 3"), "{ticks}");
    // The cancelled queued run is listed but its message is hidden.
    assert_eq!(snap.runs.last().unwrap().status, RunStatus::Cancelled);
    assert_eq!(
        snap.runs[snap.runs.len() - 2].status,
        RunStatus::Interrupted
    );
    assert!(!texts(&snap, "user_message").contains(&"too soon".to_owned()));

    // Restart: everything comes back from SQLite.
    let before = snap;
    core.shutdown();
    let (mut core, shell) = TestCore::start(&dir);
    assert_eq!(shell.projects.len(), 1);
    assert_eq!(shell.threads.len(), 1);
    let thread = &shell.threads[0];
    assert_eq!(thread.title, "list files");
    assert_eq!(thread.status, ThreadStatus::Idle);
    assert_eq!(thread.provider_thread_id.as_deref(), Some("thread-fake-1"));
    let after = core.snapshot(thread_id).await;
    assert_eq!(after.items, before.items);
    assert_eq!(after.runs, before.runs);

    // The thread keeps working after the restart (Codex thread resumed).
    core.send(thread_id, "markdown please");
    let file = core
        .added_item(|i| matches!(i.kind, ItemKind::FileChange { .. }))
        .await;
    assert!(matches!(&file.kind, ItemKind::FileChange { paths, .. } if paths == &["src/main.rs"]));
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let snap = core.snapshot(thread_id).await;
    let reply = texts(&snap, "assistant_message").join("");
    assert!(reply.contains("```rust\nfn greet"), "{reply}");
    assert!(
        !snap.items.iter().any(
            |i| matches!(&i.kind, ItemKind::SystemNotice { message } if message.contains("resume"))
        ),
        "resume should have succeeded"
    );
    core.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn crash_recovery_interrupts_unfinished_runs() {
    let dir = temp_dir("crash");
    let (mut core, _) = TestCore::start(&dir);
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, "loop");
    core.until(|e| match e {
        CoreEvent::TextDelta { chunk, .. } if chunk.starts_with("tick 5") => Some(()),
        _ => None,
    })
    .await;
    // Let at least one coalesced flush land, then "crash".
    tokio::time::sleep(Duration::from_millis(100)).await;
    core.abort();

    let (mut core, shell) = TestCore::start(&dir);
    assert_eq!(shell.threads[0].status, ThreadStatus::Idle);
    let snap = core.snapshot(thread_id).await;
    let run = &snap.runs[0];
    assert_eq!(run.status, RunStatus::Interrupted);
    assert_eq!(run.error.as_deref(), Some("process restarted"));
    assert!(run.ended_at.is_some());
    let partial = texts(&snap, "assistant_message")[0].clone();
    assert!(
        partial.starts_with("tick 1 "),
        "partial text kept: {partial}"
    );
    assert!(
        snap.items
            .iter()
            .all(|i| !matches!(i.kind, ItemKind::AssistantMessage { streaming: true }))
    );
    assert!(matches!(
        &snap.items.last().unwrap().kind,
        ItemKind::SystemNotice { message } if message.contains("exited")
    ));

    // A new turn with a pending approval, interrupted: the approval is
    // cancelled and the run ends interrupted.
    core.send(thread_id, "blocked");
    core.added_item(|i| matches!(i.kind, ItemKind::ApprovalRequest { .. }))
        .await;
    core.dispatch(Command::RunInterrupt { thread_id });
    assert_eq!(core.run_finished().await, RunStatus::Interrupted);
    let snap = core.snapshot(thread_id).await;
    assert!(snap.items.iter().any(|i| matches!(
        i.kind,
        ItemKind::ApprovalRequest {
            state: ApprovalState::Cancelled,
            ..
        }
    )));

    // A pending approval left by a crash is cancelled on recovery too.
    core.send(thread_id, "blocked again");
    core.added_item(|i| matches!(i.kind, ItemKind::ApprovalRequest { .. }))
        .await;
    core.abort();
    let (mut core, _) = TestCore::start(&dir);
    let snap = core.snapshot(thread_id).await;
    assert_eq!(snap.runs.last().unwrap().status, RunStatus::Interrupted);
    assert!(!snap.items.iter().any(|i| matches!(
        i.kind,
        ItemKind::ApprovalRequest {
            state: ApprovalState::Pending,
            ..
        }
    )));
    core.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_commands_are_rejected() {
    let dir = temp_dir("reject");
    let (mut core, _) = TestCore::start(&dir);
    let (project_id, thread_id) = core.project_and_thread(&dir).await;

    let c = core.dispatch(Command::ProjectCreate {
        project_id: ProjectId::new(),
        name: String::new(),
        path: dir.join("project").to_string_lossy().into_owned(),
    });
    assert!(core.rejected(&c).await.contains("already a project"));
    let c = core.dispatch(Command::ProjectCreate {
        project_id: ProjectId::new(),
        name: String::new(),
        path: dir.join("missing").to_string_lossy().into_owned(),
    });
    core.rejected(&c).await;
    let c = core.send(ThreadId::new(), "hi");
    assert!(core.rejected(&c).await.contains("unknown thread"));
    let c = core.send(thread_id, "   ");
    assert!(core.rejected(&c).await.contains("empty"));
    let c = core.dispatch(Command::RunInterrupt { thread_id });
    assert!(core.rejected(&c).await.contains("no turn"));
    let c = core.dispatch(Command::RuntimeRequestRespond {
        thread_id,
        item_id: ItemId::new(),
        decision: ApprovalDecision::Approve,
    });
    core.rejected(&c).await;
    let c = core.dispatch(Command::ThreadCreate {
        provider: ProviderKind::Codex,
        model: None,
        worktree: false,
        thread_id: ThreadId::new(),
        project_id: ProjectId::new(),
        title: String::new(),
    });
    assert!(core.rejected(&c).await.contains("unknown project"));

    // Rename and archive work and stick.
    core.dispatch(Command::ThreadRename {
        thread_id,
        title: "Renamed".into(),
    });
    core.dispatch(Command::ThreadArchive { thread_id });
    let c = core.send(thread_id, "hi");
    assert!(core.rejected(&c).await.contains("archived"));
    core.shutdown();
    let (core, shell) = TestCore::start(&dir);
    assert!(shell.threads.is_empty(), "archived threads are not listed");
    assert_eq!(shell.projects[0].id, project_id);
    core.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn missing_agent_fails_the_run() {
    let dir = temp_dir("missing");
    let (mut core, _) = TestCore::start_with(&dir, |c| {
        c.codex_executable = Some("/nonexistent/codex".into());
    });
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, "hi");
    assert_eq!(core.run_finished().await, RunStatus::Failed);
    let snap = core.snapshot(thread_id).await;
    assert!(matches!(
        &snap.items.last().unwrap().kind,
        ItemKind::Error { message } if message.contains("Could not start Codex")
    ));
    core.shutdown();
    let (_core, shell) = TestCore::start(&dir);
    assert_eq!(shell.threads[0].status, ThreadStatus::Failed);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn streamed_text_is_coalesced_not_written_per_delta() {
    let dir = temp_dir("coalesce");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/fixtures/resource-stream.jsonl");
    let (mut core, _) = TestCore::start_with(&dir, |c| {
        c.text_flush_interval = Duration::from_millis(200);
        c.agent_env = vec![
            ("FAKE_CODEX_REPLAY".into(), fixture.to_string_lossy().into()),
            ("FAKE_CODEX_DELAY_MS".into(), "2".into()),
        ];
    });
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, "replay");
    let mut deltas = 0usize;
    let mut streamed = String::new();
    let status = core
        .until(|e| match e {
            CoreEvent::TextDelta { chunk, .. } => {
                deltas += 1;
                streamed.push_str(chunk);
                None
            }
            CoreEvent::RunFinished { status, .. } => Some(*status),
            _ => None,
        })
        .await;
    assert_eq!(status, RunStatus::Completed);
    let snap = core.snapshot(thread_id).await;
    let body: String = snap.items.iter().map(|i| i.text.to_string()).collect();
    assert_eq!(body.len(), "replay".len() + streamed.len());
    core.shutdown();

    // Count text_appended rows in the event log: far fewer than deltas.
    let store = blongo_core::Store::open(&dir.join("data/blongo.sqlite")).unwrap();
    let appended = store
        .events_after(0, 100_000)
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::ItemTextAppended { .. }))
        .count();
    assert!(deltas > 400, "{deltas}");
    assert!(
        appended * 4 < deltas,
        "{appended} text rows for {deltas} deltas"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Quitting while an agent streams must not wait for the stop timeout: the
/// forwarders are detached first, so every driver reaps its child at once.
#[tokio::test(flavor = "current_thread")]
async fn shutdown_while_streaming_is_prompt() {
    let dir = temp_dir("quit-streaming");
    let (mut core, _) = TestCore::start(&dir);
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, "loop");
    let mut deltas = 0;
    core.until(|e| {
        if matches!(e, CoreEvent::TextDelta { .. }) {
            deltas += 1;
        }
        (deltas >= 20).then_some(())
    })
    .await;
    // Let the agent keep writing while nobody drains the core's channel.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    core.shutdown();
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "shutdown took {took:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

// ------------------------------------------------------------------------
// Recorded real Codex sessions (t3code replay fixtures, codex-cli 0.156.1)
// played by crates/blongo-harness/tests/fixtures/replay_codex.py.

struct Recording {
    transcript: PathBuf,
    log: PathBuf,
    env: Vec<(String, String)>,
}

impl Recording {
    fn new(dir: &Path, scenario: &str) -> Self {
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../blongo-harness/tests/fixtures");
        let transcript = fixtures.join(format!("t3code/{scenario}.ndjson"));
        let log = dir.join("replay-log.ndjson");
        let env = vec![
            (
                "CODEX_REPLAY_TRANSCRIPT".into(),
                transcript.to_string_lossy().into(),
            ),
            ("CODEX_REPLAY_LOG".into(), log.to_string_lossy().into()),
            (
                "CODEX_REPLAY_STATE".into(),
                dir.join("replay-state").to_string_lossy().into(),
            ),
        ];
        Self {
            transcript,
            log,
            env,
        }
    }

    fn start(&self, dir: &Path) -> TestCore {
        let replay = fake_codex().with_file_name("replay_codex.py");
        TestCore::start_with(dir, |c| {
            c.codex_executable = Some(replay);
            c.agent_env = self.env.clone();
        })
        .0
    }

    /// The recorded prompt of the n-th turn.
    fn prompt(&self, n: usize) -> String {
        std::fs::read_to_string(&self.transcript)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "expect_outbound" && e["label"] == "turn/start")
            .nth(n)
            .and_then(|e| {
                e.pointer("/frame/params/input/0/text")?
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap()
    }

    /// Actual frames the client sent for `label`.
    fn sent(&self, label: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["label"] == label)
            .map(|e| e["actual"].clone())
            .collect()
    }
}

fn command_item(snap: &ThreadSnapshot) -> &ItemKind {
    &snap
        .items
        .iter()
        .find(|i| matches!(i.kind, ItemKind::CommandExecution { .. }))
        .expect("a command item")
        .kind
}

#[tokio::test(flavor = "current_thread")]
async fn recorded_approval_turn() {
    let dir = temp_dir("rec-approval");
    let rec = Recording::new(&dir, "tool_call_read_only_on_request");
    let mut core = rec.start(&dir);
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, &rec.prompt(0));
    let approval = core
        .added_item(|i| matches!(i.kind, ItemKind::ApprovalRequest { .. }))
        .await;
    core.dispatch(Command::RuntimeRequestRespond {
        thread_id,
        item_id: approval.id,
        decision: ApprovalDecision::Approve,
    });
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let snap = core.snapshot(thread_id).await;
    assert_eq!(
        kinds(&snap),
        [
            "user_message",
            "reasoning",
            "assistant_message",
            "command_execution",
            "approval_request",
            "assistant_message"
        ]
    );
    let ItemKind::CommandExecution {
        command,
        status,
        exit_code,
        ..
    } = command_item(&snap)
    else {
        unreachable!()
    };
    assert!(
        command.contains("codex app-server approval fixture"),
        "{command}"
    );
    assert_eq!(*status, ToolStatus::Completed);
    assert_eq!(*exit_code, Some(0));
    let answers = texts(&snap, "assistant_message");
    assert!(
        answers[1].starts_with("Created or overwrote"),
        "{answers:?}"
    );
    let reasoning = texts(&snap, "reasoning");
    assert!(
        reasoning[0].contains("Waiting for write access"),
        "{reasoning:?}"
    );
    assert_eq!(
        rec.sent("item/commandExecution/requestApproval")[0]["result"]["decision"],
        "accept"
    );
    core.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn recorded_interrupts() {
    // Before any output.
    let dir = temp_dir("rec-interrupt");
    let rec = Recording::new(&dir, "turn_interrupt");
    let mut core = rec.start(&dir);
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, &rec.prompt(0));
    core.dispatch(Command::RunInterrupt { thread_id });
    assert_eq!(core.run_finished().await, RunStatus::Interrupted);
    let snap = core.snapshot(thread_id).await;
    assert_eq!(kinds(&snap), ["user_message", "system_notice"]);
    assert_eq!(snap.runs[0].status, RunStatus::Interrupted);
    let interrupt = &rec.sent("turn/interrupt")[0]["params"];
    assert_eq!(interrupt["turnId"], "01a0d5ed-38b3-7a11-9203-bd9650a02764");
    core.shutdown();
    std::fs::remove_dir_all(dir).unwrap();

    // While a command runs: the tool item ends failed, the run interrupted.
    let dir = temp_dir("rec-interrupt-tool");
    let rec = Recording::new(&dir, "turn_interrupt_mid_tool");
    let mut core = rec.start(&dir);
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, &rec.prompt(0));
    core.added_item(|i| matches!(i.kind, ItemKind::CommandExecution { .. }))
        .await;
    core.dispatch(Command::RunInterrupt { thread_id });
    assert_eq!(core.run_finished().await, RunStatus::Interrupted);
    let snap = core.snapshot(thread_id).await;
    assert!(matches!(
        command_item(&snap),
        ItemKind::CommandExecution {
            status: ToolStatus::Failed,
            exit_code: None,
            ..
        }
    ));
    assert!(matches!(
        &snap.items.last().unwrap().kind,
        ItemKind::SystemNotice { message } if message == "Turn interrupted."
    ));
    assert_eq!(rec.sent("turn/interrupt").len(), 1);
    core.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn recorded_resume_after_restart() {
    let dir = temp_dir("rec-resume");
    let rec = Recording::new(&dir, "provider_thread_resume");
    let mut core = rec.start(&dir);
    let (_, thread_id) = core.project_and_thread(&dir).await;
    core.send(thread_id, &rec.prompt(0));
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let first = core.snapshot(thread_id).await;
    assert_eq!(
        texts(&first, "assistant_message"),
        ["provider thread resume fixture first turn complete"]
    );
    core.shutdown();

    // A new Blongo process: the next message resumes the same Codex thread.
    let mut core = rec.start(&dir);
    core.send(thread_id, &rec.prompt(1));
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let snap = core.snapshot(thread_id).await;
    assert_eq!(
        kinds(&snap),
        [
            "user_message",
            "assistant_message",
            "user_message",
            "assistant_message"
        ],
        "resumed without a 'could not resume' notice"
    );
    let reply = texts(&snap, "assistant_message").pop().unwrap();
    assert!(
        reply.starts_with("provider thread resume fixture first turn complete\n"),
        "{reply}"
    );
    let resume = &rec.sent("thread/resume")[0]["params"];
    assert_eq!(resume["threadId"], "01a0d5ef-62bd-7c20-861b-0528ff7d86bd");
    assert_eq!(resume["excludeTurns"], true);
    core.shutdown();
    let (_core, shell) = TestCore::start(&dir);
    assert_eq!(
        shell.threads[0].provider_thread_id.as_deref(),
        Some("01a0d5ef-62bd-7c20-861b-0528ff7d86bd")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unopenable_database_is_reported_not_silent() {
    let dir = temp_dir("baddb");
    // A directory where the database file should be.
    let db = dir.join("data/blongo.sqlite");
    std::fs::create_dir_all(&db).unwrap();
    let (handle, mut rx) = blongo_core::spawn(CoreConfig::new(&db)).unwrap();
    match rx.blocking_recv_timeout() {
        CoreEvent::Failed { message } => assert!(message.contains("database"), "{message}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    handle.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}
