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

use crate::{ForgeSettings, PlanStep, PrLink, PrStatus, ProviderKind};

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
id_type!(
    /// A scheduled (recurring) task.
    ScheduleId
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
    /// GitHub settings (base branch, branch prefix, CI auto-fix).
    #[serde(default)]
    pub forge: ForgeSettings,
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
    #[serde(default)]
    pub provider: ProviderKind,
    /// `None`: the provider's default model.
    #[serde(default)]
    pub model: Option<String>,
    /// The thread works in its own git worktree instead of the project
    /// folder.
    #[serde(default)]
    pub worktree: Option<Worktree>,
    #[serde(default)]
    pub forked_from: Option<ThreadId>,
    /// The thread that delegated this one (an agent's `delegate_task`).
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    /// How the provider gets this thread's context at its next session,
    /// when it does not simply continue `provider_thread_id`.
    #[serde(default)]
    pub pending_context: Option<PendingContext>,
    /// The pull request of the thread's branch.
    #[serde(default)]
    pub pr: Option<PrLink>,
    /// What Blongo last saw of `pr` (`None` until the first poll).
    #[serde(default)]
    pub pr_status: Option<PrStatus>,
    /// The user unlinked the pull request: Blongo does not link the
    /// branch's pull request again on its own.
    #[serde(default)]
    pub pr_dismissed: bool,
}

impl Thread {
    /// An idle, unbound thread with no worktree.
    pub fn new(
        id: ThreadId,
        project_id: ProjectId,
        title: impl Into<String>,
        now: Timestamp,
    ) -> Self {
        Self {
            id,
            project_id,
            title: title.into(),
            status: ThreadStatus::Idle,
            archived: false,
            created_at: now,
            updated_at: now,
            provider_thread_id: None,
            provider: ProviderKind::default(),
            model: None,
            worktree: None,
            forked_from: None,
            parent_thread_id: None,
            pending_context: None,
            pr: None,
            pr_status: None,
            pr_dismissed: false,
        }
    }

    /// Where the agent works: the thread's worktree or the project folder.
    pub fn cwd<'a>(&'a self, project: &'a Project) -> &'a str {
        self.worktree.as_ref().map_or(&project.path, |w| &w.path)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub branch: String,
}

/// Context the next provider session must be given before the thread's
/// next turn (fork, provider switch, rollback).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PendingContext {
    /// The provider has no copy of this conversation: the next prompt
    /// carries a bounded transcript of it (t3code's context handoff).
    Handoff,
    /// Branch the provider's own conversation (native fork).
    Fork {
        provider_thread_id: String,
        /// Last provider turn to keep (`None`: all of it).
        up_to_turn: Option<String>,
    },
    /// Drop the provider's turns after `keep_through_turn` (native rollback).
    /// `drop_from_turn` is the first provider turn removed.
    Rewind {
        keep_through_turn: Option<String>,
        drop_from_turn: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Sent while another run was active; starts when that one ends.
    Queued,
    /// Accepted; the provider turn has not been started yet.
    Starting,
    Running,
    /// Blocked on a runtime request (approval).
    Waiting,
    Completed,
    Interrupted,
    Failed,
    /// A queued run removed before it started.
    Cancelled,
    /// Undone by a rollback: its items are hidden and its file changes
    /// reverted.
    RolledBack,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::Failed | Self::Cancelled | Self::RolledBack
        )
    }

    /// The thread status implied by a root run entering this status.
    /// `None`: the transition does not move the thread (a queued run waits
    /// behind an active one; cancel and rollback happen beside it).
    pub fn thread_status(self) -> Option<ThreadStatus> {
        match self {
            Self::Starting | Self::Running => Some(ThreadStatus::Running),
            Self::Waiting => Some(ThreadStatus::Waiting),
            Self::Completed | Self::Interrupted => Some(ThreadStatus::Idle),
            Self::Failed => Some(ThreadStatus::Failed),
            Self::Queued | Self::Cancelled | Self::RolledBack => None,
        }
    }
}

impl Run {
    /// A root run with no provider references yet.
    pub fn new(
        id: RunId,
        thread_id: ThreadId,
        status: RunStatus,
        provider: ProviderKind,
        now: Timestamp,
    ) -> Self {
        Self {
            id,
            thread_id,
            parent_run_id: None,
            status,
            created_at: now,
            ended_at: None,
            error: None,
            provider,
            provider_turn_id: None,
            checkpoint: None,
            usage: None,
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
    #[serde(default)]
    pub provider: ProviderKind,
    /// The provider's id for this turn (see `AgentEvent::ProviderTurnId`).
    #[serde(default)]
    pub provider_turn_id: Option<String>,
    /// Commit (under `refs/blongo/checkpoints/…`) of the workspace as it was
    /// just before this run started; rollback restores it.
    #[serde(default)]
    pub checkpoint: Option<String>,
    /// Tokens (and cost) the provider reported for this turn.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// What a turn cost, as the provider reported it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Part of `input_tokens` served from the provider's cache.
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Cost in millionths of a US dollar, when the provider reports one.
    #[serde(default)]
    pub cost_micros: Option<u64>,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cost_micros = match (self.cost_micros, other.cost_micros) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
    }
}

/// A recurring task: at every `cron` time, `prompt` is sent to
/// `thread_id`, or to a new thread of `project_id` when there is none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: ScheduleId,
    pub project_id: ProjectId,
    pub thread_id: Option<ThreadId>,
    /// Five-field cron (minute hour day-of-month month day-of-week), in
    /// the machine's local time.
    pub cron: String,
    pub prompt: String,
    #[serde(default)]
    pub provider: ProviderKind,
    pub enabled: bool,
    pub created_at: Timestamp,
    pub next_run_at: Option<Timestamp>,
    pub last_run_at: Option<Timestamp>,
    /// Thread the last run went to.
    pub last_thread_id: Option<ThreadId>,
    /// An agent in this thread proposed the schedule (MCP `schedule_task`):
    /// it stays disabled until the user turns it on, which approves it and
    /// clears this.
    #[serde(default)]
    pub proposed_by: Option<ThreadId>,
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
    /// The agent's plan / todo list for a run, updated in place.
    Plan {
        steps: Vec<PlanStep>,
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
            Self::Plan { .. } => "plan",
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
        #[serde(default)]
        provider: ProviderKind,
        #[serde(default)]
        model: Option<String>,
        /// Work in a new git worktree (branch `blongo/<id>`) instead of the
        /// project folder.
        #[serde(default)]
        worktree: bool,
        /// The thread that delegated this one (agent-created child).
        #[serde(default)]
        parent_thread_id: Option<ThreadId>,
    },
    /// Copy a thread (up to and including `up_to_run_id`, default: all of
    /// it) into a new one that continues independently.
    #[serde(rename = "thread.fork")]
    ThreadFork {
        source_thread_id: ThreadId,
        thread_id: ThreadId,
        #[serde(default)]
        up_to_run_id: Option<RunId>,
    },
    /// Change the thread's provider and/or model. Switching provider hands
    /// the conversation so far to the new one.
    #[serde(rename = "thread.set_provider")]
    ThreadSetProvider {
        thread_id: ThreadId,
        provider: ProviderKind,
        model: Option<String>,
    },
    /// Undo `run_id` and every later run: restore the workspace to the
    /// checkpoint taken before it and drop the runs from the conversation.
    #[serde(rename = "thread.rollback")]
    ///
    /// The restore rewrites the whole folder the thread works in. Other
    /// threads working in the same folder (or one inside or around it)
    /// must all be idle, and the user must have been told how many there
    /// are: `acknowledged_sharers` must be exactly those threads (the set
    /// shown to the user). The files as
    /// they are just before the restore are saved under
    /// `refs/blongo/pre-rollback/<thread>/<run>`.
    ThreadRollback {
        thread_id: ThreadId,
        run_id: RunId,
        #[serde(default)]
        acknowledged_sharers: Vec<ThreadId>,
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
        /// What to do when a run is already active.
        #[serde(default)]
        delivery: Delivery,
    },
    #[serde(rename = "run.interrupt")]
    RunInterrupt { thread_id: ThreadId },
    /// Remove a queued run before it starts.
    #[serde(rename = "run.cancel")]
    RunCancel { thread_id: ThreadId, run_id: RunId },
    #[serde(rename = "runtime_request.respond")]
    RuntimeRequestRespond {
        thread_id: ThreadId,
        item_id: ItemId,
        decision: ApprovalDecision,
    },
    #[serde(rename = "schedule.create")]
    ScheduleCreate {
        schedule_id: ScheduleId,
        project_id: ProjectId,
        /// Post into this thread (`None`: a new thread per run).
        thread_id: Option<ThreadId>,
        cron: String,
        prompt: String,
        #[serde(default)]
        provider: ProviderKind,
        /// Proposed by an agent in this thread: created disabled.
        #[serde(default)]
        proposed_by: Option<ThreadId>,
    },
    /// Change a schedule (`None` fields stay as they are).
    #[serde(rename = "schedule.update")]
    ScheduleUpdate {
        schedule_id: ScheduleId,
        enabled: Option<bool>,
        cron: Option<String>,
        prompt: Option<String>,
    },
    #[serde(rename = "schedule.delete")]
    ScheduleDelete { schedule_id: ScheduleId },
    /// Run a schedule now (its next time stays as it is).
    #[serde(rename = "schedule.run_now")]
    ScheduleRunNow { schedule_id: ScheduleId },
    /// Link the thread to a pull request: a URL, `owner/name#n` or `#n`
    /// (the thread's repository). Checked against GitHub before the link
    /// is recorded.
    #[serde(rename = "thread.link_pr")]
    ThreadLinkPr { thread_id: ThreadId, pr: String },
    /// Forget the thread's pull request (it is not linked again
    /// automatically).
    #[serde(rename = "thread.unlink_pr")]
    ThreadUnlinkPr { thread_id: ThreadId },
    #[serde(rename = "project.set_forge")]
    ProjectSetForge {
        project_id: ProjectId,
        settings: ForgeSettings,
    },
}

/// How a message sent while a run is active is delivered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Start now when idle; otherwise queue behind the active run.
    #[default]
    Queue,
    /// Add to the active run (the provider's steering); starts a run when
    /// idle.
    Steer,
}

impl Command {
    pub fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::ProjectCreate { .. }
            | Self::ProjectSetForge { .. }
            | Self::ScheduleCreate { .. }
            | Self::ScheduleUpdate { .. }
            | Self::ScheduleDelete { .. }
            | Self::ScheduleRunNow { .. } => None,
            Self::ThreadCreate { thread_id, .. }
            | Self::ThreadFork { thread_id, .. }
            | Self::ThreadSetProvider { thread_id, .. }
            | Self::ThreadRollback { thread_id, .. }
            | Self::ThreadRename { thread_id, .. }
            | Self::ThreadArchive { thread_id }
            | Self::MessageDispatch { thread_id, .. }
            | Self::RunInterrupt { thread_id }
            | Self::RunCancel { thread_id, .. }
            | Self::ThreadLinkPr { thread_id, .. }
            | Self::ThreadUnlinkPr { thread_id }
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
    ThreadCreated { thread: Box<Thread> },
    #[serde(rename = "thread.renamed")]
    ThreadRenamed { thread_id: ThreadId, title: String },
    #[serde(rename = "thread.archived")]
    ThreadArchived { thread_id: ThreadId },
    /// The provider's own thread id; clears `pending_context` (the new
    /// session has the context).
    #[serde(rename = "thread.provider_bound")]
    ThreadProviderBound {
        thread_id: ThreadId,
        provider_thread_id: String,
    },
    /// Provider / model changed. `provider_thread_id` is reset when the
    /// provider changes; `pending_context` says how the next session gets
    /// the conversation.
    #[serde(rename = "thread.provider_changed")]
    ThreadProviderChanged {
        thread_id: ThreadId,
        provider: ProviderKind,
        model: Option<String>,
        provider_thread_id: Option<String>,
        pending_context: Option<PendingContext>,
    },
    #[serde(rename = "run.provider_turn")]
    RunProviderTurn {
        thread_id: ThreadId,
        run_id: RunId,
        provider_turn_id: String,
    },
    /// The workspace was captured just before the run started.
    #[serde(rename = "run.checkpointed")]
    RunCheckpointed {
        thread_id: ThreadId,
        run_id: RunId,
        commit: String,
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
    /// The provider reported the turn's token usage (replaces the previous
    /// report of the same run).
    #[serde(rename = "run.usage")]
    RunUsage {
        thread_id: ThreadId,
        run_id: RunId,
        usage: Usage,
    },
    #[serde(rename = "schedule.created")]
    ScheduleCreated { schedule: Schedule },
    /// Settings or run times changed (the whole schedule as it is now).
    #[serde(rename = "schedule.updated")]
    ScheduleUpdated { schedule: Schedule },
    #[serde(rename = "schedule.deleted")]
    ScheduleDeleted { schedule_id: ScheduleId },
    /// The thread's pull request changed (`None`: unlinked). Clears the
    /// status.
    #[serde(rename = "thread.pr_linked")]
    ThreadPrLinked {
        thread_id: ThreadId,
        pr: Option<PrLink>,
        /// The user unlinked it: do not link the branch's PR again on
        /// its own.
        #[serde(default)]
        manual: bool,
    },
    /// A poll saw a different status.
    #[serde(rename = "thread.pr_status")]
    ThreadPrStatus {
        thread_id: ThreadId,
        status: Option<PrStatus>,
    },
    #[serde(rename = "project.forge_changed")]
    ProjectForgeChanged {
        project_id: ProjectId,
        settings: ForgeSettings,
    },
}

impl EventKind {
    pub fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::ProjectCreated { .. }
            | Self::ProjectForgeChanged { .. }
            | Self::ScheduleCreated { .. }
            | Self::ScheduleUpdated { .. }
            | Self::ScheduleDeleted { .. } => None,
            Self::ThreadCreated { thread } => Some(thread.id),
            Self::RunCreated { run } => Some(run.thread_id),
            Self::ItemAdded { item } | Self::ItemUpdated { item } => Some(item.thread_id),
            Self::ThreadRenamed { thread_id, .. }
            | Self::ThreadArchived { thread_id }
            | Self::ThreadProviderBound { thread_id, .. }
            | Self::ThreadProviderChanged { thread_id, .. }
            | Self::RunProviderTurn { thread_id, .. }
            | Self::RunCheckpointed { thread_id, .. }
            | Self::RunStatusChanged { thread_id, .. }
            | Self::ItemTextAppended { thread_id, .. }
            | Self::RunUsage { thread_id, .. }
            | Self::ThreadPrLinked { thread_id, .. }
            | Self::ThreadPrStatus { thread_id, .. }
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
            Self::ThreadProviderChanged { .. } => "thread.provider_changed",
            Self::RunProviderTurn { .. } => "run.provider_turn",
            Self::RunCheckpointed { .. } => "run.checkpointed",
            Self::RunCreated { .. } => "run.created",
            Self::RunStatusChanged { .. } => "run.status_changed",
            Self::ItemAdded { .. } => "item.added",
            Self::ItemUpdated { .. } => "item.updated",
            Self::ItemTextAppended { .. } => "item.text_appended",
            Self::ItemFinished { .. } => "item.finished",
            Self::RunUsage { .. } => "run.usage",
            Self::ScheduleCreated { .. } => "schedule.created",
            Self::ScheduleUpdated { .. } => "schedule.updated",
            Self::ScheduleDeleted { .. } => "schedule.deleted",
            Self::ThreadPrLinked { .. } => "thread.pr_linked",
            Self::ThreadPrStatus { .. } => "thread.pr_status",
            Self::ProjectForgeChanged { .. } => "project.forge_changed",
        }
    }
}

/// Sidebar state: every project and non-archived thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSnapshot {
    pub sequence: u64,
    pub projects: Vec<Project>,
    pub threads: Vec<Thread>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
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
            delivery: Delivery::Steer,
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
        assert_eq!(
            RunStatus::Waiting.thread_status(),
            Some(ThreadStatus::Waiting)
        );
        assert_eq!(
            RunStatus::Interrupted.thread_status(),
            Some(ThreadStatus::Idle)
        );
        assert_eq!(RunStatus::Queued.thread_status(), None);
        assert!(RunStatus::RolledBack.is_terminal());
        assert!(RunStatus::Failed.is_terminal());
        assert!(!RunStatus::Starting.is_terminal());
    }
}
