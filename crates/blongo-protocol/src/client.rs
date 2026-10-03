//! What a backend tells its client (the UI), in order.
//!
//! The in-process core produces these values directly (no serialization:
//! bodies stay shared `Arc<str>`); a remote backend decodes them from the
//! wire ([`crate::wire`]) into the very same values, so the UI does not
//! know which kind of backend it talks to.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    CommandId, DomainEvent, ItemId, ModelInfo, ProviderKind, RunStatus, ShellSnapshot, ThreadId,
    ThreadSnapshot,
};

/// Progress of an interactive provider sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginState {
    /// Open this URL in a browser to continue.
    Url(String),
    Succeeded,
    Failed(String),
}

/// Progress of the Antigravity install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallState {
    Progress(String),
    Done(PathBuf),
    Failed(String),
}

/// What a t3code import did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportReport {
    pub projects: usize,
    pub threads: usize,
    pub runs: usize,
    pub items: usize,
    /// Threads already imported earlier with nothing new.
    pub skipped_threads: usize,
    /// Threads imported earlier that got new turns or items.
    pub updated_threads: Vec<ThreadId>,
    /// Rows skipped because they could not be read.
    pub bad_rows: usize,
}

/// State of the link to a remote environment (the local core is always
/// connected and never sends these).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    /// Authenticated and streaming. `resumed`: the server replayed what was
    /// missed since the last sequence instead of sending fresh snapshots.
    Connected {
        resumed: bool,
    },
    /// Lost; the next attempt starts in `retry_in_ms`.
    Reconnecting {
        attempt: u32,
        retry_in_ms: u64,
        error: String,
    },
    /// Refused for good (bad credential, incompatible version): no retry.
    Failed(String),
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// Output of a terminal running on a (remote) backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    Output {
        id: u32,
        data: Arc<[u8]>,
    },
    Exited {
        id: u32,
    },
    /// The terminal could not be started.
    Failed {
        id: u32,
        message: String,
    },
}

/// What the core tells its client, in order.
#[derive(Clone, Debug)]
pub enum CoreEvent {
    /// Every project and live thread. Sent at startup, after an import and
    /// (remote) after a resnapshot; it replaces what the client had.
    Shell(Arc<ShellSnapshot>),
    /// Answer to `open_thread`. Every event after it is newer than the
    /// snapshot.
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
        status: RunStatus,
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
    /// Something the user should know that belongs to no open thread (for
    /// example, why an archived thread's worktree was kept).
    Notice {
        message: String,
    },
    /// Answer to an import request; a new [`CoreEvent::Shell`] follows a
    /// successful import.
    Imported(Result<ImportReport, String>),
    /// Remote backends only: the link changed state.
    Connection(ConnectionState),
    /// Remote backends only: a server-side terminal.
    Terminal(TerminalEvent),
}
