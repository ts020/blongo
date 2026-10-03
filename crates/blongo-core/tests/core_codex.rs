//! The orchestrator end to end against the offline fake `codex app-server`
//! (crates/blongo-harness/tests/fixtures/fake_codex.py): full turns with
//! approval, deny, interrupt, idempotency, restart restore and crash
//! recovery.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use blongo_core::{CoreConfig, CoreEvent, CoreHandle};
use blongo_protocol::{
    ApprovalDecision, ApprovalState, Command, CommandEnvelope, EventKind, ItemId, ItemKind,
    ProjectId, RunId, RunStatus, ShellSnapshot, ThreadId, ThreadSnapshot, ThreadStatus, ToolStatus,
    TurnItem,
};
use tokio::sync::mpsc::UnboundedReceiver;

fn fake_codex() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../blongo-harness/tests/fixtures/fake_codex.py")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blongo-core-{name}-{}", ThreadId::new()));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    dir
}

struct TestCore {
    handle: Option<CoreHandle>,
    rx: UnboundedReceiver<CoreEvent>,
    last_sequence: u64,
}

impl TestCore {
    fn start(dir: &Path) -> (Self, Arc<ShellSnapshot>) {
        Self::start_with(dir, |_| {})
    }

    fn start_with(dir: &Path, tweak: impl FnOnce(&mut CoreConfig)) -> (Self, Arc<ShellSnapshot>) {
        let mut config = CoreConfig::new(dir.join("data/blongo.sqlite"));
        config.codex_executable = Some(fake_codex());
        config.text_flush_interval = Duration::from_millis(30);
        tweak(&mut config);
        let (handle, mut rx) = blongo_core::spawn(config).unwrap();
        let shell = match rx.blocking_recv_timeout() {
            CoreEvent::Shell(shell) => shell,
            other => panic!("expected shell snapshot, got {other:?}"),
        };
        let core = Self {
            handle: Some(handle),
            rx,
            last_sequence: shell.sequence,
        };
        (core, shell)
    }

    fn handle(&self) -> &CoreHandle {
        self.handle.as_ref().unwrap()
    }

    fn dispatch(&self, command: Command) -> CommandEnvelope {
        let envelope = CommandEnvelope::new(command);
        self.handle().dispatch(envelope.clone());
        envelope
    }

    async fn next(&mut self) -> CoreEvent {
        let event = tokio::time::timeout(Duration::from_secs(20), self.rx.recv())
            .await
            .expect("timed out waiting for a core event")
            .expect("core channel closed");
        if let CoreEvent::Event(e) = &event {
            assert!(
                e.sequence > self.last_sequence,
                "sequence must increase: {} after {}",
                e.sequence,
                self.last_sequence
            );
            self.last_sequence = e.sequence;
        }
        event
    }

    /// Skip events until `f` returns `Some`.
    async fn until<T>(&mut self, mut f: impl FnMut(&CoreEvent) -> Option<T>) -> T {
        loop {
            let event = self.next().await;
            if let CoreEvent::CommandRejected { reason, .. } = &event {
                eprintln!("(rejected: {reason})");
            }
            if let Some(out) = f(&event) {
                return out;
            }
        }
    }

    async fn run_finished(&mut self) -> RunStatus {
        self.until(|e| match e {
            CoreEvent::RunFinished { status, .. } => Some(*status),
            _ => None,
        })
        .await
    }

    async fn added_item(&mut self, pred: impl Fn(&TurnItem) -> bool) -> Arc<TurnItem> {
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ItemAdded { item } if pred(item) => Some(item.clone()),
                _ => None,
            },
            _ => None,
        })
        .await
    }

    async fn snapshot(&mut self, thread_id: ThreadId) -> Arc<ThreadSnapshot> {
        self.handle().open_thread(thread_id);
        self.until(|e| match e {
            CoreEvent::Thread(s) if s.thread_id == thread_id => Some(s.clone()),
            _ => None,
        })
        .await
    }

    async fn rejected(&mut self, envelope: &CommandEnvelope) -> String {
        let id = envelope.command_id;
        self.until(|e| match e {
            CoreEvent::CommandRejected { command_id, reason } if *command_id == id => {
                Some(reason.clone())
            }
            CoreEvent::Event(ev) if ev.command_id == Some(id) => {
                panic!("command was accepted: {ev:?}")
            }
            _ => None,
        })
        .await
    }

    /// Create a project + thread; returns the thread id.
    async fn project_and_thread(&mut self, dir: &Path) -> (ProjectId, ThreadId) {
        let project_id = ProjectId::new();
        let thread_id = ThreadId::new();
        self.dispatch(Command::ProjectCreate {
            project_id,
            name: String::new(),
            path: dir.join("project").to_string_lossy().into_owned(),
        });
        self.dispatch(Command::ThreadCreate {
            thread_id,
            project_id,
            title: String::new(),
        });
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } if thread.id == thread_id => Some(()),
                _ => None,
            },
            _ => None,
        })
        .await;
        (project_id, thread_id)
    }

    fn send(&self, thread_id: ThreadId, text: &str) -> CommandEnvelope {
        self.dispatch(Command::MessageDispatch {
            thread_id,
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: text.into(),
        })
    }

    fn shutdown(mut self) {
        self.handle.take().unwrap().shutdown();
    }

    fn abort(mut self) {
        self.handle.take().unwrap().abort();
    }
}

trait RecvTimeout {
    fn blocking_recv_timeout(&mut self) -> CoreEvent;
}

impl RecvTimeout for UnboundedReceiver<CoreEvent> {
    fn blocking_recv_timeout(&mut self) -> CoreEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match self.try_recv() {
                Ok(event) => return event,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => panic!("no shell snapshot: {e:?}"),
            }
        }
    }
}

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

    // Turn 3: busy rejection, then interrupt a streaming turn.
    core.send(thread_id, "loop");
    core.until(|e| match e {
        CoreEvent::TextDelta { chunk, .. } if chunk.starts_with("tick 3") => Some(()),
        _ => None,
    })
    .await;
    let busy = core.send(thread_id, "too soon");
    assert!(core.rejected(&busy).await.contains("already running"));
    core.dispatch(Command::RunInterrupt { thread_id });
    assert_eq!(core.run_finished().await, RunStatus::Interrupted);
    let snap = core.snapshot(thread_id).await;
    let last = snap.items.last().unwrap();
    assert!(
        matches!(&last.kind, ItemKind::SystemNotice { message } if message.contains("interrupted"))
    );
    let ticks = texts(&snap, "assistant_message").last().unwrap().clone();
    assert!(ticks.starts_with("tick 1 tick 2 tick 3"), "{ticks}");
    assert_eq!(snap.runs.last().unwrap().status, RunStatus::Interrupted);

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
