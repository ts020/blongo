//! The in-process orchestrator.
//!
//! [`spawn`] starts one OS thread running a current-thread tokio runtime.
//! Everything that mutates state runs on that one task, so commands for a
//! thread are serialized by construction (a stronger form of t3code's
//! per-thread `KeyedSerialExecutor`). Clients talk to it through
//! [`CoreHandle`] and receive [`CoreEvent`]s on a channel as typed Rust
//! values — no serialization in process, bodies shared as `Arc<str>`.
//!
//! Write path (t3code orchestration v2, simplified):
//! command → validate (no I/O but the store) → one `Store::commit` with
//! events + projections + receipt + outbox → broadcast events → run the
//! outbox effects (start a provider turn, interrupt, answer an approval).
//! Provider output comes back as `AgentEvent`s and is committed the same
//! way, except streamed text: it reaches subscribers immediately as
//! [`CoreEvent::TextDelta`] and is written to SQLite coalesced, at most every
//! [`CoreConfig::text_flush_interval`] and at item / turn end.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use blongo_protocol::{
    CommandEnvelope, CommandId, DomainEvent, ItemId, ShellSnapshot, ThreadId, ThreadSnapshot,
};
use tokio::sync::mpsc;

mod orchestrator;

pub use blongo_store::Store;

/// Runtime configuration.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// SQLite database file.
    pub database: PathBuf,
    /// Codex executable; `None` uses the harness default (PATH, npm shim →
    /// native binary).
    pub codex_executable: Option<PathBuf>,
    /// Extra environment for agent processes (tests, profiling).
    pub agent_env: Vec<(String, String)>,
    /// Release an idle thread's agent process after this long.
    pub session_idle_timeout: Duration,
    /// Streamed text is committed at most this often.
    pub text_flush_interval: Duration,
    /// After an interrupt request, force-stop the agent after this long.
    pub interrupt_timeout: Duration,
}

impl CoreConfig {
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            database: database.into(),
            codex_executable: None,
            agent_env: Vec::new(),
            session_idle_timeout: Duration::from_secs(10 * 60),
            text_flush_interval: Duration::from_millis(200),
            interrupt_timeout: Duration::from_secs(10),
        }
    }

    /// `BLONGO_DATA_DIR` (default: the platform data dir + `blongo`),
    /// `BLONGO_CODEX_EXE`, `BLONGO_SESSION_IDLE_SECS`.
    pub fn from_env() -> Self {
        let dir = std::env::var_os("BLONGO_DATA_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("blongo")))
            .unwrap_or_else(|| PathBuf::from(".blongo"));
        let mut config = Self::new(dir.join("blongo.sqlite"));
        config.codex_executable = std::env::var_os("BLONGO_CODEX_EXE")
            .filter(|e| !e.is_empty())
            .map(PathBuf::from);
        if let Some(secs) = std::env::var("BLONGO_SESSION_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            config.session_idle_timeout = Duration::from_secs(secs);
        }
        config
    }
}

/// What the core tells its client, in order.
#[derive(Clone, Debug)]
pub enum CoreEvent {
    /// Sent once at startup: every project and live thread.
    Shell(Arc<ShellSnapshot>),
    /// Answer to [`CoreHandle::open_thread`]. Every event after it in the
    /// channel is newer than the snapshot.
    Thread(Arc<ThreadSnapshot>),
    /// A committed domain event (except coalesced text flushes, which the
    /// client already saw as `TextDelta`s).
    Event(Arc<DomainEvent>),
    /// Live streamed text for a text item (not yet necessarily persisted).
    TextDelta {
        thread_id: ThreadId,
        item_id: ItemId,
        chunk: Arc<str>,
    },
    /// The command was refused; nothing was written.
    CommandRejected {
        command_id: CommandId,
        reason: String,
    },
    /// The command id was already processed; nothing was written again.
    CommandDuplicate { command_id: CommandId },
    /// A root run ended (also visible as an event; convenient for tools).
    RunFinished {
        thread_id: ThreadId,
        status: blongo_protocol::RunStatus,
    },
}

pub(crate) enum Request {
    Dispatch(CommandEnvelope),
    OpenThread(ThreadId),
    Shutdown,
    /// Stop without finishing runs or flushing, like a crash (tests).
    Abort,
}

/// Client handle. Dropping it shuts the core down cleanly.
pub struct CoreHandle {
    requests: mpsc::UnboundedSender<Request>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A cheap, cloneable sender of commands (views hold these; only the owner
/// of the [`CoreHandle`] can stop the core).
#[derive(Clone)]
pub struct CoreClient {
    requests: mpsc::UnboundedSender<Request>,
}

impl CoreClient {
    /// A client connected to nothing (for view tests).
    #[doc(hidden)]
    pub fn disconnected() -> Self {
        Self {
            requests: mpsc::unbounded_channel().0,
        }
    }

    pub fn dispatch(&self, command: CommandEnvelope) {
        let _ = self.requests.send(Request::Dispatch(command));
    }

    /// Ask for a thread's snapshot ([`CoreEvent::Thread`]).
    pub fn open_thread(&self, thread_id: ThreadId) {
        let _ = self.requests.send(Request::OpenThread(thread_id));
    }
}

impl CoreHandle {
    pub fn client(&self) -> CoreClient {
        CoreClient {
            requests: self.requests.clone(),
        }
    }

    pub fn dispatch(&self, command: CommandEnvelope) {
        let _ = self.requests.send(Request::Dispatch(command));
    }

    /// Ask for a thread's snapshot ([`CoreEvent::Thread`]).
    pub fn open_thread(&self, thread_id: ThreadId) {
        let _ = self.requests.send(Request::OpenThread(thread_id));
    }

    /// Finish running turns as interrupted, flush, stop agents, and join.
    pub fn shutdown(mut self) {
        self.stop(Request::Shutdown);
    }

    /// Simulate a crash: stop at once, leaving runs unfinished in SQLite
    /// (agents are still killed). For recovery tests.
    #[doc(hidden)]
    pub fn abort(mut self) {
        self.stop(Request::Abort);
    }

    fn stop(&mut self, request: Request) {
        let _ = self.requests.send(request);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        self.stop(Request::Shutdown);
    }
}

/// Open the store, recover from an unclean previous exit, and start the
/// core thread. The first event on the returned channel is
/// [`CoreEvent::Shell`].
pub fn spawn(
    config: CoreConfig,
) -> anyhow::Result<(CoreHandle, mpsc::UnboundedReceiver<CoreEvent>)> {
    let store = Store::open(&config.database)?;
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let thread = std::thread::Builder::new()
        .name("blongo-core".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(orchestrator::run(store, config, req_rx, event_tx));
        })?;
    Ok((
        CoreHandle {
            requests: req_tx,
            thread: Some(thread),
        },
        event_rx,
    ))
}
