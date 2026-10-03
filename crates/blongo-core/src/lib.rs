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

use blongo_harness::antigravity_install::ArchivePin;
use blongo_protocol::{
    CommandEnvelope, CommandId, DomainEvent, ItemId, ModelInfo, ProviderKind, ShellSnapshot,
    ThreadId, ThreadSnapshot,
};
use tokio::sync::mpsc;

mod orchestrator;
pub mod t3_import;

pub use blongo_store::Store;

/// Runtime configuration.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// SQLite database file.
    pub database: PathBuf,
    /// Where worktrees live (`<data_dir>/worktrees/<thread>`).
    pub data_dir: PathBuf,
    /// Codex executable; `None` uses the harness default (PATH, npm shim →
    /// native binary).
    pub codex_executable: Option<PathBuf>,
    /// Claude Code executable; `None`: `BLONGO_CLAUDE_EXECUTABLE`, PATH.
    pub claude_executable: Option<PathBuf>,
    /// Antigravity ACP server; `None`: `BLONGO_ANTIGRAVITY_EXECUTABLE`, the
    /// managed install, PATH.
    pub antigravity_executable: Option<PathBuf>,
    /// Capture a git checkpoint before every run (when the thread works in
    /// a git repository).
    pub checkpoints: bool,
    /// Where and what the Antigravity installer downloads.
    pub antigravity_install: AntigravityInstall,
    /// How long an interactive sign-in may wait for the browser.
    pub login_timeout: Duration,
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
        let database = database.into();
        let data_dir = database
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            database,
            data_dir,
            codex_executable: None,
            claude_executable: None,
            antigravity_executable: None,
            checkpoints: true,
            antigravity_install: AntigravityInstall::default(),
            login_timeout: Duration::from_secs(10 * 60),
            agent_env: Vec::new(),
            session_idle_timeout: Duration::from_secs(10 * 60),
            text_flush_interval: Duration::from_millis(200),
            interrupt_timeout: Duration::from_secs(10),
        }
    }

    /// `BLONGO_DATA_DIR` (default: the platform data dir + `blongo`),
    /// `BLONGO_CODEX_EXE`, `BLONGO_CLAUDE_EXE`, `BLONGO_ANTIGRAVITY_EXE`,
    /// `BLONGO_SESSION_IDLE_SECS`.
    pub fn from_env() -> Self {
        let dir = std::env::var_os("BLONGO_DATA_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("blongo")))
            .unwrap_or_else(|| PathBuf::from(".blongo"));
        let mut config = Self::new(dir.join("blongo.sqlite"));
        let exe = |var: &str| {
            std::env::var_os(var)
                .filter(|e| !e.is_empty())
                .map(PathBuf::from)
        };
        config.codex_executable = exe("BLONGO_CODEX_EXE");
        config.claude_executable = exe("BLONGO_CLAUDE_EXE");
        config.antigravity_executable = exe("BLONGO_ANTIGRAVITY_EXE");
        if let Some(secs) = std::env::var("BLONGO_SESSION_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            config.session_idle_timeout = Duration::from_secs(secs);
        }
        config
    }

    pub fn executable(&self, provider: ProviderKind) -> Option<&PathBuf> {
        match provider {
            ProviderKind::Codex => self.codex_executable.as_ref(),
            ProviderKind::ClaudeCode => self.claude_executable.as_ref(),
            ProviderKind::Antigravity => self.antigravity_executable.as_ref(),
        }
    }
}

/// The Antigravity installer's target (defaults: the pinned archive for this
/// platform, `$XDG_DATA_HOME/blongo/antigravity-acp`, dl.google.com).
#[derive(Clone, Debug)]
pub struct AntigravityInstall {
    pub root: Option<PathBuf>,
    pub pin: Option<ArchivePin>,
    pub origin: String,
}

impl Default for AntigravityInstall {
    fn default() -> Self {
        Self {
            root: None,
            pin: None,
            origin: blongo_harness::antigravity_install::ANTIGRAVITY_ORIGIN.into(),
        }
    }
}

/// Progress of an interactive provider sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginState {
    /// Open this URL in a browser to continue.
    Url(String),
    Succeeded,
    Failed(String),
}

/// Progress of the Antigravity install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallState {
    Progress(String),
    Done(PathBuf),
    Failed(String),
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
    CommandDuplicate {
        command_id: CommandId,
    },
    /// The core could not start (or stopped) and accepts no more commands.
    Failed {
        message: String,
    },
    /// A root run ended (also visible as an event; convenient for tools).
    RunFinished {
        thread_id: ThreadId,
        status: blongo_protocol::RunStatus,
    },
    /// Models a provider offered in its handshake (not persisted).
    Models {
        provider: ProviderKind,
        models: Arc<[ModelInfo]>,
    },
    Login {
        provider: ProviderKind,
        state: LoginState,
    },
    Install(InstallState),
    /// Answer to [`CoreClient::import_t3`]; a new [`CoreEvent::Shell`]
    /// follows a successful import.
    Imported(Result<t3_import::ImportReport, String>),
}

pub(crate) enum Request {
    Dispatch(CommandEnvelope),
    OpenThread(ThreadId),
    Login(ProviderKind),
    InstallAntigravity,
    ImportT3(PathBuf),
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

    /// Start an interactive sign-in ([`CoreEvent::Login`]).
    pub fn login(&self, provider: ProviderKind) {
        let _ = self.requests.send(Request::Login(provider));
    }

    /// Download and unpack the pinned Antigravity server
    /// ([`CoreEvent::Install`]).
    pub fn install_antigravity(&self) {
        let _ = self.requests.send(Request::InstallAntigravity);
    }

    /// Import t3code's history from its `statev2.sqlite` (read-only;
    /// [`CoreEvent::Imported`]).
    pub fn import_t3(&self, source: PathBuf) {
        let _ = self.requests.send(Request::ImportT3(source));
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

/// Open the store, then start the core thread, which recovers from an
/// unclean previous exit. The first event on the returned channel is
/// [`CoreEvent::Shell`], or [`CoreEvent::Failed`] when the data cannot be
/// opened or recovered (the window can then say so instead of staying
/// blank).
pub fn spawn(
    config: CoreConfig,
) -> anyhow::Result<(CoreHandle, mpsc::UnboundedReceiver<CoreEvent>)> {
    // Opened here, not on the core thread: the connection's allocations then
    // share the caller's allocator heap instead of touching a fresh one.
    let store = Store::open(&config.database);
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
