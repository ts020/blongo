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
use std::time::Duration;

use blongo_harness::antigravity_install::ArchivePin;
pub use blongo_protocol::client::{
    ConnectionState, CoreEvent, ImportReport, InstallState, LoginState, TerminalEvent,
};
use blongo_protocol::{CommandEnvelope, ProviderKind, ThreadId};
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

pub(crate) enum Request {
    Dispatch(CommandEnvelope),
    /// Re-send the shell snapshot ([`CoreEvent::Shell`]).
    Shell,
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

    /// Ask for a fresh shell snapshot ([`CoreEvent::Shell`]); every event
    /// after it in the channel is newer.
    pub fn shell(&self) {
        let _ = self.requests.send(Request::Shell);
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
    // One core per data directory: two orchestrators on one database would
    // each "recover" the other's running turns.
    let lock = lock_database(&config.database);
    // Opened here, not on the core thread: the connection's allocations then
    // share the caller's allocator heap instead of touching a fresh one.
    let store = match &lock {
        Ok(_) => Store::open(&config.database),
        Err(err) => Err(anyhow::anyhow!("{err}")),
    };
    let (req_tx, req_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let thread = std::thread::Builder::new()
        .name("blongo-core".into())
        .spawn(move || {
            // Held for the core's lifetime.
            let _lock = lock;
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

/// Take an exclusive lock next to the database (`<db>.lock`), held until
/// the core stops. Fails when another Blongo process (an app or `blongo
/// serve`) already runs a core on the same data.
fn lock_database(database: &std::path::Path) -> Result<std::fs::File, String> {
    let path = database.with_extension("lock");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another Blongo process is already using {} (an open window or `blongo serve`); \
             set BLONGO_DATA_DIR to use separate data",
            database.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}
