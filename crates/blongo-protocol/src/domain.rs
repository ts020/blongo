//! The orchestration domain model: Project → Thread → Run → TurnItem,
//! commands, and sequenced domain events.
//!
//! A simplified form of t3code's orchestration v2 (`orchestrationV2.ts`),
//! keeping its invariants:
//!
//! - App ids are primary; provider ids (`provider_thread_id`,
//!   `provider_request_id`, `call_id`) are references stored next to them.
//! - Commands carry a client-generated `command_id`; replaying one is a no-op
//!   that returns the original receipt.
//! - Only the completion of a root run (`parent_run_id == None`) ends a turn
//!   and returns the thread to idle.
//!
//! Bodies (user / assistant / reasoning text) are stored exactly once, in the
//! projection. They travel in memory as `Arc<str>` and are deliberately not
//! part of the serialized event payloads (`#[serde(skip)]`).

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
pub use uuid::Uuid;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// A new time-ordered (v7) id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn parse(s: &str) -> Option<Self> {
                Uuid::parse_str(s).ok().map(Self)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(ProjectId);
id_type!(ThreadId);
id_type!(RunId);
id_type!(
    /// A turn item (message, reasoning block, tool call, approval, …).
    ItemId
);
id_type!(
    /// Client-generated id that makes a command idempotent.
    CommandId
);

/// Milliseconds since the Unix epoch.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn now() -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self(ms)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// Absolute folder path; the agent's working directory.
    pub path: String,
    pub created_at: Timestamp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    #[default]
    Idle,
    /// A run is starting or streaming.
    Running,
    /// A run is blocked on the user (approval).
    Waiting,
    /// The last run failed.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub status: ThreadStatus,
    pub archived: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The provider's own thread/session id (a reference, never the key).
    pub provider_thread_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Accepted; the provider turn has not been started yet.
    Starting,
    Running,
    /// Blocked on a runtime request (approval).
    Waiting,
    Completed,
    Interrupted,
    Failed,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Interrupted | Self::Failed)
    }

    /// The thread status implied by a root run in this status.
    pub fn thread_status(self) -> ThreadStatus {
        match self {
            Self::Starting | Self::Running => ThreadStatus::Running,
            Self::Waiting => ThreadStatus::Waiting,
            Self::Completed | Self::Interrupted => ThreadStatus::Idle,
            Self::Failed => ThreadStatus::Failed,
        }
    }
}

/// One user-visible turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub thread_id: ThreadId,
    /// `None` for a root run. Only root runs complete a turn.
    pub parent_run_id: Option<RunId>,
    pub status: RunStatus,
    pub created_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
    Declined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    /// The run ended (interrupt, failure, restart) before an answer.
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

/// What a turn item is. Body text is in [`TurnItem::text`], not here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AssistantMessage {
        streaming: bool,
    },
    Reasoning {
        streaming: bool,
    },
    CommandExecution {
        call_id: String,
        command: String,
        status: ToolStatus,
        /// Bounded preview of the output (the full output is not kept).
        output: String,
        exit_code: Option<i32>,
    },
    FileChange {
        call_id: String,
        paths: Vec<String>,
        status: ToolStatus,
    },
    /// MCP / web search / other provider tools.
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        status: ToolStatus,
        output: String,
    },
    ApprovalRequest {
        provider_request_id: String,
        title: String,
        detail: String,
        state: ApprovalState,
    },
    SystemNotice {
        message: String,
    },
    Error {
        message: String,
    },
}

impl ItemKind {
    /// Kinds whose body grows by streamed text.
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            Self::UserMessage | Self::AssistantMessage { .. } | Self::Reasoning { .. }
        )
    }

    pub fn tag(&self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::Reasoning { .. } => "reasoning",
            Self::CommandExecution { .. } => "command_execution",
            Self::FileChange { .. } => "file_change",
            Self::ToolCall { .. } => "tool_call",
            Self::ApprovalRequest { .. } => "approval_request",
            Self::SystemNotice { .. } => "system_notice",
            Self::Error { .. } => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnItem {
    pub id: ItemId,
    pub thread_id: ThreadId,
    pub run_id: Option<RunId>,
    /// Position in the thread's timeline (dense, per thread).
    pub ordinal: u32,
    pub created_at: Timestamp,
    pub kind: ItemKind,
    /// Body of text items. Stored once in the projection; never in the
    /// event log.
    #[serde(skip)]
    pub text: Arc<str>,
}

/// A command from a client. Ids of new entities are chosen by the client so
/// a retried command (same `command_id`) is idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub command_id: CommandId,
    pub command: Command,
}

impl CommandEnvelope {
    pub fn new(command: Command) -> Self {
        Self {
            command_id: CommandId::new(),
            command,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Command {
    #[serde(rename = "project.create")]
    ProjectCreate {
        project_id: ProjectId,
        name: String,
        path: String,
    },
    #[serde(rename = "thread.create")]
    ThreadCreate {
        thread_id: ThreadId,
        project_id: ProjectId,
        title: String,
    },
    #[serde(rename = "thread.rename")]
    ThreadRename { thread_id: ThreadId, title: String },
    #[serde(rename = "thread.archive")]
    ThreadArchive { thread_id: ThreadId },
    /// Send a user message; starts a new root run.
    #[serde(rename = "message.dispatch")]
    MessageDispatch {
        thread_id: ThreadId,
        message_id: ItemId,
        run_id: RunId,
        text: String,
    },
    #[serde(rename = "run.interrupt")]
    RunInterrupt { thread_id: ThreadId },
    #[serde(rename = "runtime_request.respond")]
    RuntimeRequestRespond {
        thread_id: ThreadId,
        item_id: ItemId,
        decision: ApprovalDecision,
    },
}

impl Command {
    pub fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::ProjectCreate { .. } => None,
            Self::ThreadCreate { thread_id, .. }
            | Self::ThreadRename { thread_id, .. }
            | Self::ThreadArchive { thread_id }
            | Self::MessageDispatch { thread_id, .. }
            | Self::RunInterrupt { thread_id }
            | Self::RuntimeRequestRespond { thread_id, .. } => Some(*thread_id),
        }
    }
}

/// A committed fact. `sequence` is global and strictly increasing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub sequence: u64,
    pub at: Timestamp,
    pub command_id: Option<CommandId>,
    pub kind: EventKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventKind {
    #[serde(rename = "project.created")]
    ProjectCreated { project: Project },
    #[serde(rename = "thread.created")]
    ThreadCreated { thread: Thread },
    #[serde(rename = "thread.renamed")]
    ThreadRenamed { thread_id: ThreadId, title: String },
    #[serde(rename = "thread.archived")]
    ThreadArchived { thread_id: ThreadId },
    #[serde(rename = "thread.provider_bound")]
    ThreadProviderBound {
        thread_id: ThreadId,
        provider_thread_id: String,
    },
    #[serde(rename = "run.created")]
    RunCreated { run: Run },
    #[serde(rename = "run.status_changed")]
    RunStatusChanged {
        thread_id: ThreadId,
        run_id: RunId,
        status: RunStatus,
        error: Option<String>,
    },
    #[serde(rename = "item.added")]
    ItemAdded { item: Arc<TurnItem> },
    /// Non-body fields of an item changed (tool status, approval state, …).
    #[serde(rename = "item.updated")]
    ItemUpdated { item: Arc<TurnItem> },
    /// Coalesced streamed text. `chunk` lives only in memory; the log keeps
    /// the resulting body length.
    #[serde(rename = "item.text_appended")]
    ItemTextAppended {
        thread_id: ThreadId,
        item_id: ItemId,
        #[serde(skip)]
        chunk: Arc<str>,
        len: u64,
    },
    /// A streaming text item is complete.
    #[serde(rename = "item.finished")]
    ItemFinished {
        thread_id: ThreadId,
        item_id: ItemId,
    },
}

impl EventKind {
    pub fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::ProjectCreated { .. } => None,
            Self::ThreadCreated { thread } => Some(thread.id),
            Self::RunCreated { run } => Some(run.thread_id),
            Self::ItemAdded { item } | Self::ItemUpdated { item } => Some(item.thread_id),
            Self::ThreadRenamed { thread_id, .. }
            | Self::ThreadArchived { thread_id }
            | Self::ThreadProviderBound { thread_id, .. }
            | Self::RunStatusChanged { thread_id, .. }
            | Self::ItemTextAppended { thread_id, .. }
            | Self::ItemFinished { thread_id, .. } => Some(*thread_id),
        }
    }

    /// Stable tag used as the event log's `kind` column.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::ProjectCreated { .. } => "project.created",
            Self::ThreadCreated { .. } => "thread.created",
            Self::ThreadRenamed { .. } => "thread.renamed",
            Self::ThreadArchived { .. } => "thread.archived",
            Self::ThreadProviderBound { .. } => "thread.provider_bound",
            Self::RunCreated { .. } => "run.created",
            Self::RunStatusChanged { .. } => "run.status_changed",
            Self::ItemAdded { .. } => "item.added",
            Self::ItemUpdated { .. } => "item.updated",
            Self::ItemTextAppended { .. } => "item.text_appended",
            Self::ItemFinished { .. } => "item.finished",
        }
    }
}

/// Sidebar state: every project and non-archived thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub sequence: u64,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
}

/// One thread's timeline as of `sequence`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub thread_id: ThreadId,
    pub sequence: u64,
    pub runs: Vec<Run>,
    pub items: Vec<Arc<TurnItem>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: ItemKind, text: &str) -> TurnItem {
        TurnItem {
            id: ItemId::new(),
            thread_id: ThreadId::new(),
            run_id: Some(RunId::new()),
            ordinal: 3,
            created_at: Timestamp(42),
            kind,
            text: text.into(),
        }
    }

    #[test]
    fn ids_are_time_ordered_and_parse() {
        let a = ThreadId::new();
        let b = ThreadId::new();
        assert!(a < b);
        assert_eq!(ThreadId::parse(&a.to_string()), Some(a));
        assert_eq!(serde_json::to_string(&a).unwrap(), format!("\"{a}\""));
    }

    #[test]
    fn command_round_trip_uses_dotted_tags() {
        let cmd = CommandEnvelope::new(Command::MessageDispatch {
            thread_id: ThreadId::new(),
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: "hi".into(),
        });
        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json["command"]["type"], "message.dispatch");
        let back: CommandEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back, cmd);

        let respond = Command::RuntimeRequestRespond {
            thread_id: ThreadId::new(),
            item_id: ItemId::new(),
            decision: ApprovalDecision::Deny,
        };
        let json = serde_json::to_string(&respond).unwrap();
        assert!(json.contains("\"runtime_request.respond\""));
        assert!(json.contains("\"deny\""));
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), respond);
    }

    #[test]
    fn event_payloads_round_trip_without_bodies() {
        let tool = item(
            ItemKind::CommandExecution {
                call_id: "c1".into(),
                command: "ls".into(),
                status: ToolStatus::Completed,
                output: "a\n".into(),
                exit_code: Some(0),
            },
            "",
        );
        let event = DomainEvent {
            sequence: 7,
            at: Timestamp(1),
            command_id: Some(CommandId::new()),
            kind: EventKind::ItemUpdated {
                item: Arc::new(tool),
            },
        };
        let back: DomainEvent =
            serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(back, event);

        // Bodies are not serialized: the projection holds them once.
        let msg = item(
            ItemKind::AssistantMessage { streaming: true },
            "secret body",
        );
        let json = serde_json::to_string(&EventKind::ItemAdded {
            item: Arc::new(msg.clone()),
        })
        .unwrap();
        assert!(!json.contains("secret body"));
        let EventKind::ItemAdded { item: back } = serde_json::from_str(&json).unwrap() else {
            panic!()
        };
        assert_eq!(back.kind, msg.kind);
        assert_eq!(&*back.text, "");

        let appended = EventKind::ItemTextAppended {
            thread_id: msg.thread_id,
            item_id: msg.id,
            chunk: "streamed".into(),
            len: 8,
        };
        let json = serde_json::to_string(&appended).unwrap();
        assert!(!json.contains("streamed\""));
        assert!(json.contains("\"len\":8"));
    }

    #[test]
    fn run_status_maps_to_thread_status() {
        assert_eq!(RunStatus::Waiting.thread_status(), ThreadStatus::Waiting);
        assert_eq!(RunStatus::Interrupted.thread_status(), ThreadStatus::Idle);
        assert!(RunStatus::Failed.is_terminal());
        assert!(!RunStatus::Starting.is_terminal());
    }
}
