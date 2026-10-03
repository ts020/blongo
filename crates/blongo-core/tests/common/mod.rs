//! Shared harness for the core integration tests: a core over fake agents
//! and helpers to wait for its events.
#![allow(dead_code, unused_imports)]

pub use std::path::{Path, PathBuf};
pub use std::sync::Arc;
pub use std::time::Duration;

pub use blongo_core::{CoreConfig, CoreEvent, CoreHandle};
pub use blongo_protocol::{
    ApprovalDecision, ApprovalState, Command, CommandEnvelope, Delivery, EventKind, ItemId,
    ItemKind, ProjectId, ProviderKind, RunId, RunStatus, ShellSnapshot, ThreadId, ThreadSnapshot,
    ThreadStatus, ToolStatus, TurnItem,
};
pub use tokio::sync::mpsc::UnboundedReceiver;

pub fn fake_codex() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../blongo-harness/tests/fixtures/fake_codex.py")
}

/// A scratch directory removed when the test ends.
pub struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn temp_dir(name: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!("blongo-core-{name}-{}", ThreadId::new()));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    TempDir(dir)
}

pub struct TestCore {
    pub handle: Option<CoreHandle>,
    pub rx: UnboundedReceiver<CoreEvent>,
    pub last_sequence: u64,
    pub db_path: Option<PathBuf>,
}

impl TestCore {
    pub fn start(dir: &Path) -> (Self, Arc<ShellSnapshot>) {
        Self::start_with(dir, |_| {})
    }

    pub fn start_with(
        dir: &Path,
        tweak: impl FnOnce(&mut CoreConfig),
    ) -> (Self, Arc<ShellSnapshot>) {
        let db_path = dir.join("data/blongo.sqlite");
        let mut config = CoreConfig::new(&db_path);
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
            db_path: Some(db_path),
        };
        (core, shell)
    }

    pub fn handle(&self) -> &CoreHandle {
        self.handle.as_ref().unwrap()
    }

    pub fn dispatch(&self, command: Command) -> CommandEnvelope {
        let envelope = CommandEnvelope::new(command);
        self.handle().dispatch(envelope.clone());
        envelope
    }

    pub async fn next(&mut self) -> CoreEvent {
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
    pub async fn until<T>(&mut self, mut f: impl FnMut(&CoreEvent) -> Option<T>) -> T {
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

    pub async fn run_finished(&mut self) -> RunStatus {
        self.until(|e| match e {
            CoreEvent::RunFinished { status, .. } => Some(*status),
            _ => None,
        })
        .await
    }

    pub async fn added_item(&mut self, pred: impl Fn(&TurnItem) -> bool) -> Arc<TurnItem> {
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ItemAdded { item } if pred(item) => Some(item.clone()),
                _ => None,
            },
            _ => None,
        })
        .await
    }

    pub async fn snapshot(&mut self, thread_id: ThreadId) -> Arc<ThreadSnapshot> {
        self.handle().open_thread(thread_id);
        self.until(|e| match e {
            CoreEvent::Thread(s) if s.thread_id == thread_id => Some(s.clone()),
            _ => None,
        })
        .await
    }

    pub async fn rejected(&mut self, envelope: &CommandEnvelope) -> String {
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
    pub async fn project_and_thread(&mut self, dir: &Path) -> (ProjectId, ThreadId) {
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
            provider: ProviderKind::Codex,
            model: None,
            worktree: false,
            parent_thread_id: None,
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

    pub fn send(&self, thread_id: ThreadId, text: &str) -> CommandEnvelope {
        self.send_with(thread_id, text, Delivery::Queue)
    }

    pub fn send_with(
        &self,
        thread_id: ThreadId,
        text: &str,
        delivery: Delivery,
    ) -> CommandEnvelope {
        self.dispatch(Command::MessageDispatch {
            thread_id,
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: text.into(),
            delivery,
        })
    }

    pub fn shutdown(mut self) {
        self.handle.take().unwrap().shutdown();
    }

    pub fn abort(mut self) {
        self.handle.take().unwrap().abort();
    }
}

pub trait RecvTimeout {
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

pub fn kinds(snapshot: &ThreadSnapshot) -> Vec<&'static str> {
    snapshot.items.iter().map(|i| i.kind.tag()).collect()
}

pub fn texts(snapshot: &ThreadSnapshot, tag: &str) -> Vec<String> {
    snapshot
        .items
        .iter()
        .filter(|i| i.kind.tag() == tag)
        .map(|i| i.text.to_string())
        .collect()
}

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../blongo-harness/tests/fixtures")
        .join(name)
}
