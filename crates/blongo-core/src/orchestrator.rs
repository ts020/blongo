//! The core task: command handling, provider event translation, coalesced
//! text persistence, effects, recovery and session lifecycle.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use blongo_harness::{
    ApprovalDecision as HarnessDecision, Session, SessionConfig, StartOptions, acp,
    antigravity_install,
};
use blongo_protocol::workspace::{DiffScope, Query, QueryId, QueryReply};
use blongo_protocol::{
    AgentEvent, ApprovalDecision, ApprovalState, Command, CommandEnvelope, CommandId, Delivery,
    EventKind, ItemId, ItemKind, PendingContext, PlanStep, Project, ProjectId, ProviderKind, Run,
    RunId, RunStatus, Schedule, ScheduleId, ShellSnapshot, Thread, ThreadId, ThreadSnapshot,
    Timestamp, ToolStatus, TurnItem, TurnStatus, Usage, Worktree,
};
use blongo_store::{Batch, CommitOutcome, Effect, EffectStatus, OutboxRow, Store};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::cron::Cron;
use crate::mcp::{self, McpCall, McpServer};
use crate::workspace::{self, Plan};
use crate::{
    ApprovalPolicy, CoreConfig, CoreEvent, ImportReport, InstallState, LoginState, Request,
};

mod autofix;
mod forge;
mod merge;
mod tools;

/// Default title of a new thread; replaced by the first message.
pub const DEFAULT_TITLE: &str = "New thread";
const MAX_OUTPUT_PREVIEW: usize = 4 * 1024;
const MAX_TITLE: usize = 48;
/// Upper bound of the transcript a context handoff puts in front of the
/// next prompt.
const MAX_HANDOFF_CHARS: usize = 24_000;

/// One message from a session's forwarding task. `event == None`: the agent
/// session ended.
struct SessionMsg {
    thread_id: ThreadId,
    generation: u64,
    event: Option<AgentEvent>,
}

struct LiveSession {
    session: Session,
    generation: u64,
    provider: ProviderKind,
    /// The session's access to Blongo's MCP server (revoked on drop).
    _mcp: Option<mcp::Grant>,
    /// Task copying the session's events into the core loop. Aborted
    /// before the session is shut down: a forwarder blocked on a full core
    /// channel would otherwise keep the driver blocked on its own send, and
    /// the shutdown would wait for the timeout.
    forwarder: tokio::task::JoinHandle<()>,
}

impl LiveSession {
    /// Detach from the core loop and reap the agent in the background.
    fn release(self) {
        self.forwarder.abort();
        tokio::spawn(self.session.shutdown());
    }
}

const SESSION_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Assistant,
    Reasoning,
}

struct OpenText {
    item_id: ItemId,
    kind: TextKind,
    /// Streamed but not yet committed.
    pending: String,
    /// Committed body length in bytes.
    len: u64,
}

struct ActiveRun {
    run_id: RunId,
    status: RunStatus,
    open_text: Option<OpenText>,
    /// Provider call id → item.
    tools: HashMap<String, Arc<TurnItem>>,
    /// Pending approval items.
    approvals: HashMap<ItemId, Arc<TurnItem>>,
    interrupt_deadline: Option<Instant>,
    /// The run's plan item (updated in place).
    plan: Option<Arc<TurnItem>>,
    provider_turn_id: Option<String>,
    usage: Option<Usage>,
}

/// A message waiting behind the active run.
#[derive(Clone, Copy)]
struct Queued {
    run_id: RunId,
    message_id: ItemId,
}

/// Runtime changes that only apply once their command's batch committed.
enum Post {
    Release(ThreadId),
    Enqueue(ThreadId, Queued),
    Dequeue(ThreadId, RunId),
    /// Run a schedule now (`schedule.run_now`).
    Fire(ScheduleId),
}

/// Who waits for a command's outcome.
enum Reply {
    /// A client: a refusal is a `CommandRejected`; success is the events.
    Client,
    /// The core itself (scheduled runs): a refusal becomes a notice.
    Internal,
    /// A query that sent a message (a pull request draft or fix): a
    /// refusal answers it; a draft is answered when its turn ends, a fix
    /// once the message is in.
    Query(QueryId),
    /// An automatic CI fix's message.
    Fix(RunId),
    /// An agent's MCP tool call.
    Mcp {
        tx: oneshot::Sender<Result<Value, String>>,
        then: Then,
    },
}

/// What an MCP call does once its command committed.
enum Then {
    /// Answer with this.
    Value(Value),
    /// Answer once the thread's work is done.
    Wait(ThreadId),
    /// Run this command next (its reply carries the answer on).
    Chain(Box<Pending>),
}

struct Pending {
    command: CommandEnvelope,
    reply: Reply,
}

impl Pending {
    fn client(command: CommandEnvelope) -> Self {
        Self {
            command,
            reply: Reply::Client,
        }
    }

    fn new(command: Command, reply: Reply) -> Self {
        Self {
            command: CommandEnvelope {
                command_id: CommandId::new(),
                command,
            },
            reply,
        }
    }
}

/// Work waiting for its turn: everything for one thread runs in order, and
/// nothing overtakes work that needs the whole core (`Key::Global`).
enum Deferred {
    Dispatch(Pending),
    Query(QueryId, Query),
    Import(PathBuf),
}

/// What a piece of work must have to itself while it runs off the loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    /// Nothing (decided on the loop at once).
    None,
    Thread(ThreadId),
    /// Every thread (rollback rewrites a shared folder, import rewrites
    /// the caches, a branch switch changes a shared folder).
    Global,
}

/// I/O a command needs before it can be decided, resolved on the loop and
/// run as a job.
enum PrepPlan {
    Worktree {
        project_path: PathBuf,
        path: PathBuf,
        branch: String,
        /// Start from the remote's base branch (`None`: the project's HEAD).
        base: Option<forge::BasePlan>,
    },
    Rollback {
        cwd: PathBuf,
        pre: String,
        commit: String,
        sharers: u32,
    },
    /// Look up and check a pull request (`thread.link_pr`).
    LinkPr {
        ctx: forge::ForgeCtx,
        cwd: PathBuf,
        reference: (Option<String>, Option<String>, u64),
    },
}

/// A job finished off the loop.
enum JobDone {
    Prepared {
        pending: Box<Pending>,
        key: Key,
        result: Result<Prepared, String>,
    },
    Checkpoint {
        thread_id: ThreadId,
        run_id: RunId,
        message_id: ItemId,
        /// `Ok(None)`: not a git repository.
        result: Result<Option<String>, String>,
    },
    Mutation {
        id: QueryId,
        key: Key,
        result: Result<QueryReply, String>,
    },
    Imported(anyhow::Result<ImportReport>),
    /// Background work that only held its key (archive clean-up).
    Released(Key),
    /// A pull request poll ended.
    Forge(Box<forge::ForgeDone>),
    /// A PR tab query ended.
    ForgeQuery(Box<forge::QueryDone>),
    ForgeFix(Box<autofix::FixDone>),
    ForgeMerge(Box<merge::MergeDone>),
}

/// Work a command needs done (with I/O) before it can be decided.
#[derive(Default)]
struct Prepared {
    worktree: Option<Worktree>,
    /// Rollback restored the workspace from this checkpoint.
    restored: Option<String>,
    /// Rollback saved the files it replaced under this ref.
    pre_rollback: Option<String>,
    /// Other threads working in the folder the rollback rewrote.
    sharers: u32,
    /// The pull request `thread.link_pr` found.
    pr: Option<Box<forge::Found>>,
    /// Said in a new thread (its worktree did not start where asked).
    notice: Option<String>,
    /// The base branch a new worktree started from (cached for when
    /// GitHub cannot be asked).
    base: Option<String>,
}

/// Runtime state for a thread that has (or had) an agent session or run.
struct ThreadRt {
    next_ordinal: u32,
    session: Option<LiveSession>,
    run: Option<ActiveRun>,
    idle_since: Option<Instant>,
    queue: VecDeque<Queued>,
}

pub(crate) struct Orchestrator {
    store: Store,
    config: CoreConfig,
    out: mpsc::UnboundedSender<CoreEvent>,
    projects: HashMap<ProjectId, Project>,
    threads: HashMap<ThreadId, Thread>,
    rt: HashMap<ThreadId, ThreadRt>,
    session_tx: mpsc::Sender<SessionMsg>,
    generation: u64,
    flush_deadline: Option<Instant>,
    post: Vec<Post>,
    /// Threads whose run just ended and whose queue may start.
    ready: Vec<ThreadId>,
    /// An Antigravity install is running (one at a time).
    installing: Arc<std::sync::atomic::AtomicBool>,
    /// Work waiting behind a busy key, in arrival order.
    deferred: VecDeque<Deferred>,
    /// Threads with a job running off the loop.
    busy: HashSet<ThreadId>,
    /// A `Key::Global` job is running.
    global_busy: bool,
    jobs: mpsc::UnboundedSender<JobDone>,
    schedules: HashMap<ScheduleId, Schedule>,
    mcp: Option<McpServer>,
    /// MCP calls waiting for a thread's work to end.
    waiters: HashMap<ThreadId, Vec<oneshot::Sender<Result<Value, String>>>>,
    /// Runs an agent asked for with `t3_thread_send` on a thread it did
    /// not create: run → (thread, sender, project). They count toward the
    /// spawn limits while the run is waiting or working.
    agent_runs: HashMap<RunId, (ThreadId, ThreadId, ProjectId)>,
    forge: forge::ForgeRt,
}

pub(crate) async fn run(
    store: anyhow::Result<Store>,
    config: CoreConfig,
    mut requests: mpsc::UnboundedReceiver<Request>,
    out: mpsc::UnboundedSender<CoreEvent>,
) {
    let store = match store {
        Ok(store) => store,
        Err(err) => {
            eprintln!(
                "blongo-core: cannot open {}: {err:#}",
                config.database.display()
            );
            let _ = out.send(CoreEvent::Failed {
                message: format!(
                    "Blongo could not open its database {}: {err:#}",
                    config.database.display()
                ),
            });
            return;
        }
    };
    // Bounded: a core busy committing back-pressures the agents' stdout
    // instead of queueing their output in memory.
    let (session_tx, mut session_rx) = mpsc::channel(SESSION_CHANNEL_CAPACITY);
    let (jobs, mut jobs_rx) = mpsc::unbounded_channel();
    let (mcp_tx, mut mcp_rx) = mpsc::unbounded_channel::<McpCall>();
    let mcp = if config.mcp {
        McpServer::start(&config.data_dir, mcp_tx)
    } else {
        None
    };
    let mut core = Orchestrator {
        store,
        config,
        out,
        projects: HashMap::new(),
        threads: HashMap::new(),
        rt: HashMap::new(),
        session_tx,
        generation: 0,
        flush_deadline: None,
        post: Vec::new(),
        ready: Vec::new(),
        installing: Default::default(),
        deferred: VecDeque::new(),
        busy: HashSet::new(),
        global_busy: false,
        jobs,
        schedules: HashMap::new(),
        mcp,
        waiters: HashMap::new(),
        agent_runs: HashMap::new(),
        forge: forge::ForgeRt::default(),
    };
    if let Err(err) = core.start() {
        eprintln!("blongo-core: startup failed: {err:#}");
        core.emit(CoreEvent::Failed {
            message: format!("Blongo could not load its data: {err:#}"),
        });
        return;
    }
    loop {
        let timer = core.next_timer();
        tokio::select! {
            request = requests.recv() => match request {
                Some(Request::Dispatch(command)) => {
                    core.deferred.push_back(Deferred::Dispatch(Pending::client(command)));
                }
                Some(Request::Query(id, query)) if query.is_mutation() => {
                    core.deferred.push_back(Deferred::Query(id, query));
                }
                Some(Request::Query(id, query)) => core.query(id, query),
                Some(Request::Configure(settings)) => core.config.settings = settings,
                Some(Request::OpenThread(thread_id)) => core.open_thread(thread_id),
                Some(Request::Shell) => match core.shell_snapshot() {
                    Ok(shell) => core.emit(CoreEvent::Shell(Arc::new(shell))),
                    Err(err) => eprintln!("blongo-core: shell snapshot failed: {err:#}"),
                },
                Some(Request::Login(provider)) => core.login(provider),
                Some(Request::InstallAntigravity) => core.install_antigravity(),
                Some(Request::ImportT3(source)) => core.deferred.push_back(Deferred::Import(source)),
                Some(Request::Abort) => {
                    // No forwarder may block on a full channel now.
                    session_rx.close();
                    core.abort().await;
                    return;
                }
                Some(Request::Shutdown) | None => {
                    session_rx.close();
                    core.shutdown().await;
                    return;
                }
            },
            Some(msg) = session_rx.recv() => core.on_session_msg(msg).await,
            Some(done) = jobs_rx.recv() => core.on_job_done(done).await,
            Some(call) = mcp_rx.recv() => core.on_mcp(call),
            _ = sleep_until(timer), if timer.is_some() => core.on_timer().await,
        }
        core.drain().await;
        core.start_queued().await;
        core.flush_notices();
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    }
}

impl Orchestrator {
    fn start(&mut self) -> anyhow::Result<()> {
        self.recover()?;
        for project in self.store.projects()? {
            self.projects.insert(project.id, project);
        }
        for thread in self.store.threads(false)? {
            self.threads.insert(thread.id, thread);
        }
        for schedule in self.store.schedules()? {
            self.schedules.insert(schedule.id, schedule);
        }
        self.forge_start();
        let snapshot = self.shell_snapshot()?;
        let _ = self.out.send(CoreEvent::Shell(Arc::new(snapshot)));
        Ok(())
    }

    /// Bring state left by a previous process to rest: its agent processes
    /// are gone, so unfinished runs become interrupted (never hung), pending
    /// approvals are cancelled, streaming items end, and process-bound
    /// outbox effects are dropped instead of replayed.
    fn recover(&mut self) -> anyhow::Result<()> {
        for row in self.store.pending_effects()? {
            self.store
                .set_effect_status(row.id, EffectStatus::Dropped)?;
        }
        self.store.prune_effects()?;
        for run in self.store.unfinished_runs()? {
            let mut events = Vec::new();
            for item in self.store.open_items(run.id)? {
                events.extend(close_item_event(&item));
            }
            let ordinal = self.store.next_ordinal(run.thread_id)?;
            let message = if run.status == RunStatus::Queued {
                "Not sent: Blongo exited before this queued message started."
            } else {
                "Interrupted: Blongo exited before this turn finished."
            };
            events.push(EventKind::ItemAdded {
                item: Arc::new(TurnItem {
                    id: ItemId::new(),
                    thread_id: run.thread_id,
                    run_id: Some(run.id),
                    ordinal,
                    created_at: Timestamp::now(),
                    kind: ItemKind::SystemNotice {
                        message: message.into(),
                    },
                    text: "".into(),
                }),
            });
            events.push(EventKind::RunStatusChanged {
                thread_id: run.thread_id,
                run_id: run.id,
                status: RunStatus::Interrupted,
                error: Some("process restarted".into()),
            });
            self.store.commit(Batch {
                command_id: None,
                events,
                effects: vec![],
            })?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- output

    fn emit(&self, event: CoreEvent) {
        let _ = self.out.send(event);
    }

    /// Commit and broadcast. Returns the outbox rows to execute.
    fn commit(&mut self, batch: Batch) -> anyhow::Result<Vec<OutboxRow>> {
        match self.store.commit(batch)? {
            CommitOutcome::Committed { events, outbox } => {
                for event in events {
                    self.track(&event.kind);
                    if !matches!(event.kind, EventKind::ItemTextAppended { .. }) {
                        self.emit(CoreEvent::Event(Arc::new(event)));
                    }
                }
                Ok(outbox)
            }
            CommitOutcome::Duplicate(receipt) => {
                self.emit(CoreEvent::CommandDuplicate {
                    command_id: receipt.command_id,
                });
                Ok(Vec::new())
            }
        }
    }

    /// Commit events not caused by a client command (provider output,
    /// timers). A failure here is a bug or a broken disk; log it and keep
    /// the core alive.
    fn commit_events(&mut self, events: Vec<EventKind>) {
        if events.is_empty() {
            return;
        }
        if let Err(err) = self.commit(Batch {
            command_id: None,
            events,
            effects: vec![],
        }) {
            eprintln!("blongo-core: commit failed: {err:#}");
        }
    }

    /// Keep the in-memory project/thread caches in sync with commits.
    fn track(&mut self, event: &EventKind) {
        match event {
            EventKind::ProjectCreated { project } => {
                self.projects.insert(project.id, project.clone());
            }
            EventKind::ThreadCreated { thread } => {
                self.threads.insert(thread.id, (**thread).clone());
            }
            EventKind::ThreadRenamed { thread_id, title } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.title = title.clone();
                }
            }
            EventKind::ThreadArchived { thread_id } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.archived = true;
                }
            }
            EventKind::ThreadProviderBound {
                thread_id,
                provider_thread_id,
            } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.provider_thread_id = Some(provider_thread_id.clone());
                    t.pending_context = None;
                }
            }
            EventKind::ThreadProviderChanged {
                thread_id,
                provider,
                model,
                provider_thread_id,
                pending_context,
            } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.provider = *provider;
                    t.model = model.clone();
                    t.provider_thread_id = provider_thread_id.clone();
                    t.pending_context = pending_context.clone();
                }
            }
            EventKind::RunStatusChanged {
                thread_id, status, ..
            } => {
                if let Some(t) = self.threads.get_mut(thread_id)
                    && let Some(status) = status.thread_status()
                {
                    t.status = status;
                }
            }
            EventKind::RunCreated { run } => {
                if let Some(t) = self.threads.get_mut(&run.thread_id)
                    && let Some(status) = run.status.thread_status()
                {
                    t.status = status;
                }
            }
            EventKind::ScheduleCreated { schedule } | EventKind::ScheduleUpdated { schedule } => {
                self.schedules.insert(schedule.id, schedule.clone());
            }
            EventKind::ScheduleDeleted { schedule_id } => {
                self.schedules.remove(schedule_id);
            }
            EventKind::ThreadPrLinked {
                thread_id,
                pr,
                manual,
            } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.pr = pr.clone();
                    t.pr_status = None;
                    t.pr_dismissed = pr.is_none() && *manual;
                }
            }
            EventKind::ThreadPrStatus { thread_id, status } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.pr_status = status.clone();
                }
            }
            EventKind::ThreadBranchRenamed { thread_id, branch } => {
                if let Some(w) = self
                    .threads
                    .get_mut(thread_id)
                    .and_then(|t| t.worktree.as_mut())
                {
                    w.branch = branch.clone();
                }
            }
            EventKind::ProjectForgeChanged {
                project_id,
                settings,
            } => {
                if let Some(p) = self.projects.get_mut(project_id) {
                    p.forge = settings.clone();
                }
            }
            _ => {}
        }
        self.forge_track(event);
    }

    fn rt(&mut self, thread_id: ThreadId) -> anyhow::Result<&mut ThreadRt> {
        if !self.rt.contains_key(&thread_id) {
            let next_ordinal = self.store.next_ordinal(thread_id)?;
            self.rt.insert(
                thread_id,
                ThreadRt {
                    next_ordinal,
                    session: None,
                    run: None,
                    idle_since: None,
                    queue: VecDeque::new(),
                },
            );
        }
        Ok(self.rt.get_mut(&thread_id).expect("inserted"))
    }

    fn new_item(
        &mut self,
        thread_id: ThreadId,
        run_id: Option<RunId>,
        kind: ItemKind,
        text: &str,
    ) -> anyhow::Result<TurnItem> {
        let rt = self.rt(thread_id)?;
        let ordinal = rt.next_ordinal;
        rt.next_ordinal += 1;
        Ok(TurnItem {
            id: ItemId::new(),
            thread_id,
            run_id,
            ordinal,
            created_at: Timestamp::now(),
            kind,
            text: text.into(),
        })
    }

    fn active_run(&mut self, thread_id: ThreadId) -> Option<&mut ActiveRun> {
        self.rt.get_mut(&thread_id).and_then(|rt| rt.run.as_mut())
    }

    // ------------------------------------------------------------ sequencing

    /// What `item` must have to itself.
    fn deferred_key(&self, item: &Deferred) -> Key {
        match item {
            Deferred::Dispatch(pending) => command_key(&pending.command.command),
            Deferred::Query(_, Query::GitSwitch { .. } | Query::PrCreate { .. }) => Key::Global,
            Deferred::Query(_, query) => Key::Thread(query.thread_id()),
            Deferred::Import(_) => Key::Global,
        }
    }

    /// Run every deferred item whose key is free, oldest first. An item
    /// that must wait blocks later items of its thread (all of them for a
    /// global one), so per-thread order is the arrival order.
    async fn drain(&mut self) {
        'scan: loop {
            if self.global_busy || self.deferred.is_empty() {
                return;
            }
            let mut blocked = HashSet::new();
            for i in 0..self.deferred.len() {
                let key = self.deferred_key(&self.deferred[i]);
                let runnable = match key {
                    Key::None => true,
                    Key::Thread(t) => !self.busy.contains(&t) && !blocked.contains(&t),
                    Key::Global => self.busy.is_empty() && blocked.is_empty(),
                };
                if runnable {
                    let item = self.deferred.remove(i).expect("in range");
                    self.run_deferred(item, key).await;
                    continue 'scan;
                }
                match key {
                    Key::Global => return,
                    Key::Thread(t) => {
                        blocked.insert(t);
                    }
                    Key::None => {}
                }
            }
            return;
        }
    }

    async fn run_deferred(&mut self, item: Deferred, key: Key) {
        match item {
            Deferred::Dispatch(pending) => self.dispatch(pending, key).await,
            Deferred::Query(id, query) => self.mutate(id, query, key),
            Deferred::Import(source) => self.import_t3(source),
        }
    }

    /// Spawn a job holding `key` until it reports to the loop.
    fn spawn_job<F>(&mut self, key: Key, job: F)
    where
        F: std::future::Future<Output = JobDone> + Send + 'static,
    {
        match key {
            Key::None => {}
            Key::Thread(t) => {
                self.busy.insert(t);
            }
            Key::Global => self.global_busy = true,
        }
        let jobs = self.jobs.clone();
        tokio::spawn(async move {
            let _ = jobs.send(job.await);
        });
    }

    fn release_key(&mut self, key: Key) {
        match key {
            Key::None => {}
            Key::Thread(t) => {
                self.busy.remove(&t);
                // A queued message may have waited for the job.
                self.ready.push(t);
            }
            Key::Global => self.global_busy = false,
        }
    }

    async fn on_job_done(&mut self, done: JobDone) {
        // The thread whose job ended: its `t3_thread_wait` callers are
        // answered once nothing else holds it (a wait registered while
        // only a job ran would never be answered otherwise).
        let settled = match &done {
            JobDone::Prepared {
                key: Key::Thread(t),
                ..
            }
            | JobDone::Mutation {
                key: Key::Thread(t),
                ..
            }
            | JobDone::Released(Key::Thread(t)) => Some(*t),
            JobDone::Checkpoint { thread_id, .. } => Some(*thread_id),
            _ => None,
        };
        self.handle_job_done(done).await;
        if let Some(t) = settled
            && !self.is_busy(t)
            && !self.busy.contains(&t)
        {
            self.resolve_waiters(t);
        }
    }

    async fn handle_job_done(&mut self, done: JobDone) {
        match done {
            JobDone::Prepared {
                pending,
                key,
                result,
            } => {
                self.release_key(key);
                match result {
                    Ok(prepared) => self.decide_and_commit(*pending, prepared).await,
                    Err(reason) => self.refuse(*pending, reason),
                }
            }
            JobDone::Checkpoint {
                thread_id,
                run_id,
                message_id,
                result,
            } => {
                self.release_key(Key::Thread(thread_id));
                // The run may have ended meanwhile (shutdown, a dead agent).
                if self.active_run(thread_id).map(|r| r.run_id) != Some(run_id) {
                    return;
                }
                match result {
                    Ok(Some(commit)) => self.commit_events(vec![EventKind::RunCheckpointed {
                        thread_id,
                        run_id,
                        commit,
                    }]),
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!("blongo-core: checkpoint failed: {err}");
                        self.add_item(
                            thread_id,
                            run_id,
                            ItemKind::SystemNotice {
                                message: format!(
                                    "No checkpoint was taken for this turn, so rolling it back \
                                     will not restore files: {err}"
                                ),
                            },
                        );
                    }
                }
                if let Err(err) = self.continue_turn(thread_id, run_id, message_id).await {
                    eprintln!("blongo-core: turn failed to start: {err:#}");
                }
            }
            JobDone::Mutation { id, key, result } => {
                self.release_key(key);
                self.emit(CoreEvent::Reply { id, result });
            }
            JobDone::Imported(result) => {
                self.release_key(Key::Global);
                self.after_import(result);
            }
            JobDone::Released(key) => self.release_key(key),
            JobDone::Forge(done) => self.forge_done(*done),
            JobDone::ForgeQuery(done) => self.forge_query_done(*done),
            JobDone::ForgeFix(done) => self.fix_done(*done),
            JobDone::ForgeMerge(done) => self.merge_done(*done),
        }
    }

    // -------------------------------------------------------------- commands

    fn refuse(&mut self, pending: Pending, reason: String) {
        match pending.reply {
            Reply::Client => self.emit(CoreEvent::CommandRejected {
                command_id: pending.command.command_id,
                reason,
            }),
            Reply::Internal => {
                eprintln!("blongo-core: scheduled command refused: {reason}");
                self.emit(CoreEvent::Notice {
                    message: format!("A scheduled task could not run: {reason}"),
                });
            }
            Reply::Fix(run_id) => self.fix_refused(run_id, &reason),
            Reply::Query(id) => {
                self.forge.drafts.retain(|_, q| *q != id);
                self.forge.answers.remove(&id);
                self.emit(CoreEvent::Reply {
                    id,
                    result: Err(reason),
                });
            }
            Reply::Mcp { tx, .. } => {
                let _ = tx.send(Err(reason));
            }
        }
    }

    /// The command committed (or was a duplicate): tell whoever waits.
    fn succeed(&mut self, reply: Reply) {
        if let Reply::Fix(run_id) = reply {
            return self.fix_sent(run_id);
        }
        if let Reply::Query(id) = reply {
            // A draft answers when its turn ends; a fix now.
            if !self.forge.drafts.values().any(|q| *q == id) {
                let text = self
                    .forge
                    .answers
                    .remove(&id)
                    .unwrap_or_else(|| "sent to the agent".into());
                self.emit(CoreEvent::Reply {
                    id,
                    result: Ok(QueryReply::Done(text)),
                });
            }
            return;
        }
        let Reply::Mcp { tx, then } = reply else {
            return;
        };
        match then {
            Then::Value(value) => {
                let _ = tx.send(Ok(value));
            }
            Then::Wait(thread_id) => self.wait_for(thread_id, tx),
            Then::Chain(mut next) => {
                // The chained command answers the call.
                if let Reply::Mcp { tx: slot, .. } = &mut next.reply {
                    *slot = tx;
                }
                // Next in line for its key, ahead of later arrivals.
                self.deferred.push_front(Deferred::Dispatch(*next));
            }
        }
    }

    async fn dispatch(&mut self, pending: Pending, key: Key) {
        match self.store.receipt(pending.command.command_id) {
            Ok(Some(_)) => {
                self.emit(CoreEvent::CommandDuplicate {
                    command_id: pending.command.command_id,
                });
                return self.succeed(pending.reply);
            }
            Ok(None) => {}
            Err(err) => return self.refuse(pending, format!("{err:#}")),
        }
        match self.prepare(&pending.command.command) {
            Err(reason) => self.refuse(pending, reason),
            Ok(None) => self.decide_and_commit(pending, Prepared::default()).await,
            Ok(Some(plan)) => {
                // The worktree / restore runs off the loop; the command's
                // thread (or the whole core, for a rollback) waits for it.
                let key = if key == Key::None { Key::Global } else { key };
                self.spawn_job(key, async move {
                    let result = run_prep(plan).await;
                    JobDone::Prepared {
                        pending: Box::new(pending),
                        key,
                        result,
                    }
                });
            }
        }
    }

    async fn decide_and_commit(&mut self, pending: Pending, prepared: Prepared) {
        let command = &pending.command;
        self.post.clear();
        let worktree = prepared.worktree.clone();
        // Files already restored by a rollback that is now refused: say
        // where the replaced ones are.
        let restored_note = prepared.pre_rollback.as_ref().map(|pre| {
            format!(
                " The files were already restored; how they were before is saved in {pre} \
                 (`git restore --source={pre} --worktree -- .`)."
            )
        });
        let batch = match self.decide(command, prepared) {
            Ok(batch) => batch,
            Err(reason) => {
                self.post.clear();
                self.discard_worktree(&command.command, worktree);
                let reason = reason + restored_note.as_deref().unwrap_or_default();
                return self.refuse(pending, reason);
            }
        };
        match self.commit(batch) {
            Ok(outbox) => {
                for post in std::mem::take(&mut self.post) {
                    self.apply_post(post);
                }
                if let Command::ThreadArchive { thread_id } = &command.command {
                    self.clean_up_archived(*thread_id);
                }
                for row in outbox {
                    self.run_effect(row).await;
                }
                self.succeed(pending.reply);
            }
            Err(err) => {
                self.post.clear();
                self.discard_worktree(&command.command, worktree);
                // Ordinals handed out for the failed batch are not reused,
                // which is harmless (ordinals only need to be increasing).
                let reason = format!("{err:#}") + restored_note.as_deref().unwrap_or_default();
                self.refuse(pending, reason);
            }
        }
    }

    fn apply_post(&mut self, post: Post) {
        match post {
            Post::Release(thread_id) => {
                self.release_session(thread_id);
                if let Some(rt) = self.rt.get(&thread_id)
                    && rt.run.is_none()
                    && rt.queue.is_empty()
                {
                    self.rt.remove(&thread_id);
                }
            }
            Post::Enqueue(thread_id, queued) => {
                if let Ok(rt) = self.rt(thread_id) {
                    rt.queue.push_back(queued);
                }
            }
            Post::Dequeue(thread_id, run_id) => {
                if let Some(rt) = self.rt.get_mut(&thread_id) {
                    rt.queue.retain(|q| q.run_id != run_id);
                }
            }
            Post::Fire(schedule_id) => self.fire_schedule(schedule_id, false),
        }
    }

    /// The I/O a command needs before it can be decided (create a
    /// worktree, restore a checkpoint), validated just enough to not do
    /// that work for a command that is going to be refused anyway.
    fn prepare(&mut self, command: &Command) -> Result<Option<PrepPlan>, String> {
        match command {
            Command::ThreadCreate {
                thread_id,
                project_id,
                worktree: true,
                ..
            } => {
                let project = self.projects.get(project_id).ok_or("unknown project")?;
                if self.threads.contains_key(thread_id) {
                    return Err("thread already exists".into());
                }
                let project_path = PathBuf::from(&project.path);
                let id = thread_id.0.simple().to_string();
                let branch = format!("{}{}", project.forge.branch_prefix, &id[id.len() - 12..]);
                let base = self.base_plan(project);
                let path = self
                    .config
                    .data_dir
                    .join("worktrees")
                    .join(thread_id.to_string());
                Ok(Some(PrepPlan::Worktree {
                    project_path,
                    path,
                    branch,
                    base,
                }))
            }
            Command::ThreadRollback {
                thread_id,
                run_id,
                acknowledged_sharers,
            } => {
                let (thread, cwd) = self.thread_cwd(*thread_id)?;
                self.ensure_idle(&thread)?;
                let run = self
                    .store
                    .run(*run_id)
                    .map_err(|e| format!("{e:#}"))?
                    .filter(|r| r.thread_id == *thread_id)
                    .ok_or("unknown turn")?;
                if !matches!(
                    run.status,
                    RunStatus::Completed | RunStatus::Interrupted | RunStatus::Failed
                ) {
                    return Err("this turn cannot be rolled back".into());
                }
                let Some(commit) = &run.checkpoint else {
                    return Ok(None);
                };
                // The restore rewrites the whole folder: every other thread
                // working there must be idle, and the user must know how
                // many there are.
                let sharers = self.folder_sharers(*thread_id, &cwd);
                if let Some(busy) = sharers.iter().find(|t| self.is_busy(t.id)) {
                    return Err(format!(
                        "\"{}\" works in the same folder and is running; wait for it \
                         to finish before rolling back",
                        busy.title
                    ));
                }
                let n = sharers.len() as u32;
                let ids: HashSet<ThreadId> = sharers.iter().map(|t| t.id).collect();
                let acked: HashSet<ThreadId> = acknowledged_sharers.iter().copied().collect();
                if ids != acked {
                    return Err(format!(
                        "{n} other thread{} work{} in this folder and will see its files \
                         change; confirm the rollback for all of them",
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "s" } else { "" }
                    ));
                }
                Ok(Some(PrepPlan::Rollback {
                    cwd: PathBuf::from(cwd),
                    pre: format!(
                        "{}/{thread_id}/{run_id}",
                        blongo_git::PRE_ROLLBACK_REF_PREFIX
                    ),
                    commit: commit.clone(),
                    sharers: n,
                }))
            }
            Command::ThreadLinkPr { thread_id, pr } => self.prepare_link(*thread_id, pr).map(Some),
            _ => Ok(None),
        }
    }

    /// An archived thread's checkpoint refs go (and any left by threads
    /// archived before), except those a live fork's copied runs still use;
    /// its pre-rollback refs stay (they hold files a rollback replaced).
    /// Its worktree goes only when no other live thread works in it and
    /// nothing in it would be lost, ignored files included (the branch
    /// stays either way); otherwise the user is told why it was kept. The
    /// git work runs off the loop; later work waits for it (it may remove
    /// a folder and refs other threads' work would touch).
    fn clean_up_archived(&mut self, thread_id: ThreadId) {
        let Some(thread) = self.threads.get(&thread_id).cloned() else {
            return;
        };
        let Some(project) = self.projects.get(&thread.project_id).cloned() else {
            return;
        };
        let cwd = PathBuf::from(thread.cwd(&project));
        let live: Vec<Thread> = self
            .threads
            .values()
            .filter(|t| t.id != thread_id && !t.archived)
            .cloned()
            .collect();
        let keep: HashSet<String> = live
            .iter()
            .flat_map(|t| self.store.runs(t.id).unwrap_or_default())
            .filter_map(|r| r.checkpoint)
            .collect();
        // This thread's refs, and those of threads archived earlier that
        // were kept for a fork that may be gone now.
        let archived: Vec<ThreadId> = self
            .store
            .threads(true)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.archived && t.project_id == thread.project_id)
            .map(|t| t.id)
            .chain([thread_id])
            .collect();
        // Folders of the other live threads, to tell whether one still
        // works in the worktree.
        let others: Vec<(String, String)> = live
            .iter()
            .filter_map(|t| {
                self.projects
                    .get(&t.project_id)
                    .map(|p| (t.title.clone(), t.cwd(p).to_owned()))
            })
            .collect();
        let worktrees = self.config.data_dir.join("worktrees");
        let out = self.out.clone();
        self.spawn_job(Key::Global, async move {
            clean_up(cwd, archived, keep, thread, project, others, worktrees, out).await;
            JobDone::Released(Key::Global)
        });
    }
}

/// The git side of [`Orchestrator::clean_up_archived`].
#[allow(clippy::too_many_arguments)]
async fn clean_up(
    cwd: PathBuf,
    archived: Vec<ThreadId>,
    keep: HashSet<String>,
    thread: Thread,
    project: Project,
    others: Vec<(String, String)>,
    worktrees: PathBuf,
    out: mpsc::UnboundedSender<CoreEvent>,
) {
    if blongo_git::work_tree_root(&cwd).await.is_none() {
        return;
    }
    for id in archived {
        blongo_git::delete_thread_refs(&cwd, &id.to_string(), &keep).await;
    }
    let Some(worktree) = &thread.worktree else {
        return;
    };
    // A nested project works in a subfolder: the worktree is its top.
    let top = match blongo_git::work_tree_root(Path::new(&worktree.path)).await {
        Some(top) => canonical(&top.to_string_lossy()),
        None => canonical(&worktree.path),
    };
    let users: Vec<&str> = others
        .iter()
        .filter(|(_, cwd)| canonical(cwd).starts_with(&top))
        .map(|(title, _)| title.as_str())
        .collect();
    let kept = if !users.is_empty() {
        Some(format!(
            "{} still work{} in it",
            users.join(", "),
            if users.len() == 1 { "s" } else { "" }
        ))
    } else {
        blongo_git::remove_pristine_worktree(
            Path::new(&project.path),
            Path::new(&worktree.path),
            &worktrees,
        )
        .await
        .err()
        .map(|e| format!("{e:#}"))
    };
    if let Some(why) = kept {
        let _ = out.send(CoreEvent::Notice {
            message: format!(
                "Kept the worktree of \"{}\" at {}: {why}.",
                thread.title, worktree.path
            ),
        });
        return;
    }
    // The branch of a merged pull request goes too, when it is exactly
    // what was merged.
    if let (Some(link), Some(status)) = (&thread.pr, &thread.pr_status)
        && status.state == blongo_protocol::PrState::Merged
        && !link.read_only
        && link.head_branch == worktree.branch
        && thread.parent_thread_id.is_none()
        && thread.forked_from.is_none()
        && let Err(err) = blongo_git::delete_merged_branch(
            Path::new(&project.path),
            &worktree.branch,
            &status.head_sha,
        )
        .await
    {
        eprintln!("blongo-core: kept branch {}: {err:#}", worktree.branch);
    }
}

impl Orchestrator {
    /// Remove a worktree prepared for a command that was then refused.
    fn discard_worktree(&self, command: &Command, worktree: Option<Worktree>) {
        let (Some(worktree), Command::ThreadCreate { project_id, .. }) = (worktree, command) else {
            return;
        };
        let Some(project) = self.projects.get(project_id) else {
            return;
        };
        let repo = PathBuf::from(&project.path);
        let root = self.config.data_dir.join("worktrees");
        tokio::spawn(async move {
            let _ = blongo_git::remove_worktree(&repo, Path::new(&worktree.path), &root).await;
        });
    }

    fn thread_cwd(&self, thread_id: ThreadId) -> Result<(Thread, String), String> {
        let thread = self.live_thread(thread_id)?.clone();
        let project = self
            .projects
            .get(&thread.project_id)
            .ok_or("unknown project")?;
        let cwd = thread.cwd(project).to_owned();
        Ok((thread, cwd))
    }

    fn is_busy(&self, thread_id: ThreadId) -> bool {
        self.rt
            .get(&thread_id)
            .is_some_and(|rt| rt.run.is_some() || !rt.queue.is_empty())
    }

    /// Live threads other than `thread_id` whose folder is `cwd`, inside it
    /// or around it (compared canonically): a restore of `cwd` changes their
    /// files too.
    fn folder_sharers(&self, thread_id: ThreadId, cwd: &str) -> Vec<Thread> {
        let mine = canonical(cwd);
        self.threads
            .values()
            .filter(|t| t.id != thread_id && !t.archived)
            .filter(|t| {
                self.projects.get(&t.project_id).is_some_and(|p| {
                    let theirs = canonical(t.cwd(p));
                    theirs.starts_with(&mine) || mine.starts_with(&theirs)
                })
            })
            .cloned()
            .collect()
    }

    fn ensure_idle(&self, thread: &Thread) -> Result<(), String> {
        if self.is_busy(thread.id) {
            Err("wait for the running turn (and queued messages) to finish".into())
        } else {
            Ok(())
        }
    }

    /// Runs that are part of the conversation (root, not rolled back or
    /// cancelled), oldest first.
    fn visible_runs(&self, thread_id: ThreadId) -> Result<Vec<Run>, String> {
        Ok(self
            .store
            .runs(thread_id)
            .map_err(|e| format!("{e:#}"))?
            .into_iter()
            .filter(|r| {
                r.parent_run_id.is_none()
                    && !matches!(r.status, RunStatus::RolledBack | RunStatus::Cancelled)
            })
            .collect())
    }

    /// Validate a command and decide its events and effects (no side
    /// effects besides reading state and handing out ordinals; runtime
    /// changes wait in `self.post` for the commit).
    fn decide(&mut self, envelope: &CommandEnvelope, prepared: Prepared) -> Result<Batch, String> {
        let mut batch = Batch::for_command(envelope.command_id);
        let now = Timestamp::now();
        match &envelope.command {
            Command::ProjectCreate {
                project_id,
                name,
                path,
            } => {
                if self.projects.contains_key(project_id) {
                    return Err("project already exists".into());
                }
                let path = path.trim();
                if path.is_empty() {
                    return Err("enter a folder path".into());
                }
                let expanded = expand_home(path);
                let canonical = std::fs::canonicalize(&expanded)
                    .map_err(|e| format!("{}: {e}", expanded.display()))?;
                if !canonical.is_dir() {
                    return Err(format!("{} is not a folder", canonical.display()));
                }
                let path = canonical.to_string_lossy().into_owned();
                if self.projects.values().any(|p| p.path == path) {
                    return Err(format!("{path} is already a project"));
                }
                let name = if name.trim().is_empty() {
                    canonical
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone())
                } else {
                    name.trim().to_owned()
                };
                batch.events.push(EventKind::ProjectCreated {
                    project: Project {
                        id: *project_id,
                        name,
                        path,
                        created_at: now,
                        forge: Default::default(),
                    },
                });
            }
            Command::ThreadCreate {
                thread_id,
                project_id,
                title,
                provider,
                model,
                worktree: _,
                parent_thread_id,
            } => {
                if !self.projects.contains_key(project_id) {
                    return Err("unknown project".into());
                }
                if self.threads.contains_key(thread_id) {
                    return Err("thread already exists".into());
                }
                let parent = match parent_thread_id {
                    Some(id) => {
                        let parent = self.live_thread(*id)?;
                        if parent.project_id != *project_id {
                            return Err("the parent thread is in another project".into());
                        }
                        Some(parent.clone())
                    }
                    None => None,
                };
                let title = title.trim();
                let mut thread = Thread::new(
                    *thread_id,
                    *project_id,
                    if title.is_empty() {
                        DEFAULT_TITLE
                    } else {
                        title
                    },
                    now,
                );
                thread.provider = *provider;
                thread.model = model.clone().filter(|m| !m.trim().is_empty()).or_else(|| {
                    self.config
                        .settings
                        .default_models
                        .iter()
                        .find(|(p, _)| p == provider)
                        .map(|(_, m)| m.clone())
                });
                thread.worktree = prepared.worktree;
                if let Some(base) = &prepared.base {
                    self.cache_base(*project_id, base);
                }
                let notice = prepared.notice.map(|message| TurnItem {
                    id: ItemId::new(),
                    thread_id: *thread_id,
                    run_id: None,
                    ordinal: 0,
                    created_at: now,
                    kind: ItemKind::SystemNotice { message },
                    text: "".into(),
                });
                if let Some(parent) = parent {
                    // A delegated task works where its parent works.
                    thread.parent_thread_id = Some(parent.id);
                    if thread.worktree.is_none() {
                        thread.worktree = parent.worktree.clone();
                    }
                }
                batch.events.push(EventKind::ThreadCreated {
                    thread: Box::new(thread),
                });
                if let Some(item) = notice {
                    batch.events.push(EventKind::ItemAdded {
                        item: Arc::new(item),
                    });
                }
            }
            Command::ThreadFork {
                source_thread_id,
                thread_id,
                up_to_run_id,
            } => self.decide_fork(
                &mut batch,
                *source_thread_id,
                *thread_id,
                *up_to_run_id,
                now,
            )?,
            Command::ThreadSetProvider {
                thread_id,
                provider,
                model,
            } => {
                let thread = self.live_thread(*thread_id)?.clone();
                self.ensure_idle(&thread)?;
                let model = model.clone().filter(|m| !m.trim().is_empty());
                if thread.provider == *provider && thread.model == model {
                    return Ok(batch);
                }
                let switching = thread.provider != *provider;
                let has_history = !self.visible_runs(*thread_id)?.is_empty();
                let (provider_thread_id, pending_context) = if switching {
                    (None, has_history.then_some(PendingContext::Handoff))
                } else {
                    (
                        thread.provider_thread_id.clone(),
                        thread.pending_context.clone(),
                    )
                };
                batch.events.push(EventKind::ThreadProviderChanged {
                    thread_id: *thread_id,
                    provider: *provider,
                    model,
                    provider_thread_id,
                    pending_context,
                });
                if switching && has_history {
                    let item = self
                        .new_item(
                            *thread_id,
                            None,
                            ItemKind::SystemNotice {
                                message: format!(
                                    "Switched from {} to {}. The conversation so far is \
                                     handed over with the next message.",
                                    thread.provider.label(),
                                    provider.label()
                                ),
                            },
                            "",
                        )
                        .map_err(|e| format!("{e:#}"))?;
                    batch.events.push(EventKind::ItemAdded {
                        item: Arc::new(item),
                    });
                }
                // The next turn starts a session with the new settings.
                self.post.push(Post::Release(*thread_id));
            }
            Command::ThreadRollback {
                thread_id, run_id, ..
            } => self.decide_rollback(&mut batch, *thread_id, *run_id, &prepared)?,
            Command::ThreadLinkPr { thread_id, .. } => {
                self.decide_link(&mut batch, *thread_id, prepared.pr)?;
            }
            Command::ThreadUnlinkPr { thread_id } => {
                self.decide_unlink(&mut batch, *thread_id)?;
            }
            Command::ProjectSetForge {
                project_id,
                settings,
            } => {
                self.decide_set_forge(&mut batch, *project_id, settings)?;
            }
            Command::ThreadRename { thread_id, title } => {
                self.live_thread(*thread_id)?;
                let title = title.trim();
                if title.is_empty() {
                    return Err("title is empty".into());
                }
                batch.events.push(EventKind::ThreadRenamed {
                    thread_id: *thread_id,
                    title: title.into(),
                });
            }
            Command::ThreadArchive { thread_id } => {
                let thread = self.live_thread(*thread_id)?.clone();
                self.ensure_idle(&thread)?;
                batch.events.push(EventKind::ThreadArchived {
                    thread_id: *thread_id,
                });
                self.post.push(Post::Release(*thread_id));
            }
            Command::MessageDispatch {
                thread_id,
                message_id,
                run_id,
                text,
                delivery,
            } => {
                let thread = self.live_thread(*thread_id)?.clone();
                if text.trim().is_empty() {
                    return Err("message is empty".into());
                }
                let active = self.active_run(*thread_id).map(|r| r.run_id);
                match (active, delivery) {
                    (Some(active), Delivery::Steer) => {
                        // Into the running turn: no new run.
                        batch.events.extend(self.close_text(*thread_id));
                        let item = self
                            .new_item(*thread_id, Some(active), ItemKind::UserMessage, text)
                            .map_err(|e| format!("{e:#}"))?;
                        batch.events.push(EventKind::ItemAdded {
                            item: Arc::new(TurnItem {
                                id: *message_id,
                                ..item
                            }),
                        });
                        batch.effects.push(Effect::ProviderSteer {
                            thread_id: *thread_id,
                            run_id: active,
                            message_id: *message_id,
                        });
                    }
                    (Some(_), Delivery::Queue) => {
                        batch.events.push(EventKind::RunCreated {
                            run: Run::new(
                                *run_id,
                                *thread_id,
                                RunStatus::Queued,
                                thread.provider,
                                now,
                            ),
                        });
                        let item = self
                            .new_item(*thread_id, Some(*run_id), ItemKind::UserMessage, text)
                            .map_err(|e| format!("{e:#}"))?;
                        batch.events.push(EventKind::ItemAdded {
                            item: Arc::new(TurnItem {
                                id: *message_id,
                                ..item
                            }),
                        });
                        self.post.push(Post::Enqueue(
                            *thread_id,
                            Queued {
                                run_id: *run_id,
                                message_id: *message_id,
                            },
                        ));
                    }
                    (None, _) => {
                        batch.events.push(EventKind::RunCreated {
                            run: Run::new(
                                *run_id,
                                *thread_id,
                                RunStatus::Starting,
                                thread.provider,
                                now,
                            ),
                        });
                        let item = self
                            .new_item(*thread_id, Some(*run_id), ItemKind::UserMessage, text)
                            .map_err(|e| format!("{e:#}"))?;
                        batch.events.push(EventKind::ItemAdded {
                            item: Arc::new(TurnItem {
                                id: *message_id,
                                ..item
                            }),
                        });
                        batch.effects.push(Effect::ProviderTurnStart {
                            thread_id: *thread_id,
                            run_id: *run_id,
                            message_id: *message_id,
                        });
                    }
                }
                if thread.title == DEFAULT_TITLE {
                    batch.events.push(EventKind::ThreadRenamed {
                        thread_id: *thread_id,
                        title: title_from(text),
                    });
                }
            }
            Command::RunInterrupt { thread_id } => {
                self.live_thread(*thread_id)?;
                let Some(run) = self.active_run(*thread_id) else {
                    return Err("no turn is running".into());
                };
                batch.effects.push(Effect::ProviderInterrupt {
                    thread_id: *thread_id,
                    run_id: run.run_id,
                });
            }
            Command::RunCancel { thread_id, run_id } => {
                self.live_thread(*thread_id)?;
                let queued = self
                    .rt
                    .get(thread_id)
                    .is_some_and(|rt| rt.queue.iter().any(|q| q.run_id == *run_id));
                if !queued {
                    return Err("this message is not queued".into());
                }
                batch.events.push(EventKind::RunStatusChanged {
                    thread_id: *thread_id,
                    run_id: *run_id,
                    status: RunStatus::Cancelled,
                    error: None,
                });
                self.post.push(Post::Dequeue(*thread_id, *run_id));
            }
            Command::RuntimeRequestRespond {
                thread_id,
                item_id,
                decision,
            } => {
                self.live_thread(*thread_id)?;
                let Some(run) = self.active_run(*thread_id) else {
                    return Err("no turn is running".into());
                };
                let Some(item) = run.approvals.get(item_id) else {
                    return Err("this request is no longer pending".into());
                };
                let ItemKind::ApprovalRequest {
                    provider_request_id,
                    ..
                } = &item.kind
                else {
                    return Err("not an approval request".into());
                };
                let approve = *decision == ApprovalDecision::Approve;
                let state = if approve {
                    ApprovalState::Approved
                } else {
                    ApprovalState::Denied
                };
                batch.effects.push(Effect::RuntimeRequestRespond {
                    thread_id: *thread_id,
                    run_id: run.run_id,
                    provider_request_id: provider_request_id.clone(),
                    approve,
                });
                batch.events.push(EventKind::ItemUpdated {
                    item: Arc::new(with_approval_state(item, state)),
                });
                if run.approvals.len() == 1 {
                    batch.events.push(EventKind::RunStatusChanged {
                        thread_id: *thread_id,
                        run_id: run.run_id,
                        status: RunStatus::Running,
                        error: None,
                    });
                }
            }
            Command::ScheduleCreate {
                schedule_id,
                project_id,
                thread_id,
                cron,
                prompt,
                provider,
                proposed_by,
            } => {
                if self.schedules.contains_key(schedule_id) {
                    return Err("schedule already exists".into());
                }
                if let Some(agent) = proposed_by {
                    self.check_proposal(*agent, *project_id, cron)?;
                }
                if !self.projects.contains_key(project_id) {
                    return Err("unknown project".into());
                }
                if let Some(thread_id) = thread_id
                    && self.live_thread(*thread_id)?.project_id != *project_id
                {
                    return Err("the thread is in another project".into());
                }
                let parsed = Cron::parse(cron)?;
                if prompt.trim().is_empty() {
                    return Err("the prompt is empty".into());
                }
                let next_run_at = parsed.next_after(now);
                if next_run_at.is_none() {
                    return Err(format!("`{cron}` never matches"));
                }
                batch.events.push(EventKind::ScheduleCreated {
                    schedule: Schedule {
                        id: *schedule_id,
                        project_id: *project_id,
                        thread_id: *thread_id,
                        cron: cron.trim().to_owned(),
                        prompt: prompt.trim().to_owned(),
                        provider: *provider,
                        // A proposal waits for the user to turn it on.
                        enabled: proposed_by.is_none(),
                        created_at: now,
                        next_run_at: next_run_at.filter(|_| proposed_by.is_none()),
                        last_run_at: None,
                        last_thread_id: None,
                        proposed_by: *proposed_by,
                    },
                });
            }
            Command::ScheduleUpdate {
                schedule_id,
                enabled,
                cron,
                prompt,
            } => {
                let mut schedule = self
                    .schedules
                    .get(schedule_id)
                    .cloned()
                    .ok_or("unknown schedule")?;
                if let Some(cron) = cron {
                    Cron::parse(cron)?;
                    schedule.cron = cron.trim().to_owned();
                }
                if let Some(prompt) = prompt {
                    if prompt.trim().is_empty() {
                        return Err("the prompt is empty".into());
                    }
                    schedule.prompt = prompt.trim().to_owned();
                }
                if let Some(enabled) = enabled {
                    schedule.enabled = *enabled;
                    // Turning a proposal on is the user's approval.
                    if *enabled {
                        schedule.proposed_by = None;
                    }
                }
                schedule.next_run_at = if schedule.enabled {
                    Cron::parse(&schedule.cron)?.next_run(now, schedule.last_run_at)
                } else {
                    None
                };
                batch.events.push(EventKind::ScheduleUpdated { schedule });
            }
            Command::ScheduleDelete { schedule_id } => {
                if !self.schedules.contains_key(schedule_id) {
                    return Err("unknown schedule".into());
                }
                batch.events.push(EventKind::ScheduleDeleted {
                    schedule_id: *schedule_id,
                });
            }
            Command::ScheduleRunNow { schedule_id } => {
                if !self.schedules.contains_key(schedule_id) {
                    return Err("unknown schedule".into());
                }
                self.post.push(Post::Fire(*schedule_id));
            }
        }
        Ok(batch)
    }

    /// A new thread holding a copy of the source's conversation (up to and
    /// including `up_to`). The provider continues it natively when it can
    /// branch its own conversation, otherwise by a context handoff.
    fn decide_fork(
        &mut self,
        batch: &mut Batch,
        source_id: ThreadId,
        thread_id: ThreadId,
        up_to: Option<RunId>,
        now: Timestamp,
    ) -> Result<(), String> {
        let source = self.live_thread(source_id)?.clone();
        if self.threads.contains_key(&thread_id) {
            return Err("thread already exists".into());
        }
        let mut runs: Vec<Run> = self
            .visible_runs(source_id)?
            .into_iter()
            .filter(|r| r.status.is_terminal())
            .collect();
        if let Some(up_to) = up_to {
            let Some(pos) = runs.iter().position(|r| r.id == up_to) else {
                return Err("that turn cannot be forked from".into());
            };
            runs.truncate(pos + 1);
        }
        let caps = source.provider.capabilities();
        let pending_context = match runs.last() {
            None => None,
            // A pending rewind does not matter: the fork names the last turn
            // it keeps, which is never a rolled back one.
            Some(last) => match (&source.provider_thread_id, &source.pending_context) {
                (Some(pid), None | Some(PendingContext::Rewind { .. }))
                    if caps.native_fork && last.provider_turn_id.is_some() =>
                {
                    Some(PendingContext::Fork {
                        provider_thread_id: pid.clone(),
                        up_to_turn: last.provider_turn_id.clone(),
                    })
                }
                _ => Some(PendingContext::Handoff),
            },
        };
        let mut thread = Thread::new(thread_id, source.project_id, fork_title(&source.title), now);
        thread.provider = source.provider;
        thread.model = source.model.clone();
        thread.worktree = source.worktree.clone();
        thread.forked_from = Some(source_id);
        thread.pending_context = pending_context;
        batch.events.push(EventKind::ThreadCreated {
            thread: Box::new(thread),
        });

        let mut run_map = HashMap::new();
        for run in &runs {
            let id = RunId::new();
            run_map.insert(run.id, id);
            batch.events.push(EventKind::RunCreated {
                run: Run {
                    id,
                    thread_id,
                    ..run.clone()
                },
            });
        }
        let items = self.store.items(source_id).map_err(|e| format!("{e:#}"))?;
        let last_ordinal = items
            .iter()
            .filter(|i| i.run_id.is_some_and(|r| run_map.contains_key(&r)))
            .map(|i| i.ordinal)
            .max();
        for item in items {
            let run_id = match item.run_id {
                Some(r) => match run_map.get(&r) {
                    Some(new) => Some(*new),
                    None => continue,
                },
                // Thread-level notices before the cut.
                None if last_ordinal.is_some_and(|last| item.ordinal < last) => None,
                None => continue,
            };
            batch.events.push(EventKind::ItemAdded {
                item: Arc::new(TurnItem {
                    id: ItemId::new(),
                    thread_id,
                    run_id,
                    ..(*item).clone()
                }),
            });
        }
        Ok(())
    }

    /// Undo `run_id` and every later run.
    fn decide_rollback(
        &mut self,
        batch: &mut Batch,
        thread_id: ThreadId,
        run_id: RunId,
        prepared: &Prepared,
    ) -> Result<(), String> {
        let thread = self.live_thread(thread_id)?.clone();
        self.ensure_idle(&thread)?;
        let runs = self.visible_runs(thread_id)?;
        let Some(pos) = runs.iter().position(|r| r.id == run_id) else {
            return Err("unknown turn".into());
        };
        let (kept, dropped) = runs.split_at(pos);
        let target = &dropped[0];
        for run in dropped {
            batch.events.push(EventKind::RunStatusChanged {
                thread_id,
                run_id: run.id,
                status: RunStatus::RolledBack,
                error: None,
            });
        }
        // The newest kept turn the provider can name (a failed turn may have
        // none); without any, a native rewind would start from nothing.
        let keep_through = kept.iter().rev().find_map(|r| r.provider_turn_id.clone());
        let rewindable = kept.is_empty() || keep_through.is_some();
        let caps = thread.provider.capabilities();
        let live = self
            .rt
            .get(&thread_id)
            .is_some_and(|rt| rt.session.is_some());
        let mut live_rewind = None;
        let (provider_thread_id, pending_context) = match &thread.pending_context {
            // The provider never saw this thread's turns natively yet: the
            // fork point moves back with the rollback.
            Some(PendingContext::Fork {
                provider_thread_id, ..
            }) => match (&keep_through, kept.is_empty()) {
                (_, true) => (None, None),
                (Some(turn), false) => (
                    None,
                    Some(PendingContext::Fork {
                        provider_thread_id: provider_thread_id.clone(),
                        up_to_turn: Some(turn.clone()),
                    }),
                ),
                (None, false) => (None, Some(PendingContext::Handoff)),
            },
            Some(PendingContext::Handoff) => (None, Some(PendingContext::Handoff)),
            _ => match (&thread.provider_thread_id, &target.provider_turn_id) {
                (Some(pid), Some(drop_from)) if caps.native_rollback && rewindable => {
                    if caps.live_rollback && live {
                        live_rewind = Some(drop_from.clone());
                        (Some(pid.clone()), None)
                    } else {
                        (
                            Some(pid.clone()),
                            Some(PendingContext::Rewind {
                                keep_through_turn: keep_through.clone(),
                                drop_from_turn: Some(drop_from.clone()),
                            }),
                        )
                    }
                }
                // No native rollback: a fresh provider conversation that
                // gets what is left as a handoff.
                _ => (None, (!kept.is_empty()).then_some(PendingContext::Handoff)),
            },
        };
        batch.events.push(EventKind::ThreadProviderChanged {
            thread_id,
            provider: thread.provider,
            model: thread.model.clone(),
            provider_thread_id,
            pending_context,
        });
        let turns = dropped.len();
        let files = match &prepared.pre_rollback {
            Some(pre) if prepared.restored.is_some() => {
                let others = match prepared.sharers {
                    0 => String::new(),
                    1 => " This also changed the files of 1 other thread in this folder.".into(),
                    n => {
                        format!(" This also changed the files of {n} other threads in this folder.")
                    }
                };
                format!(
                    "Files were restored to how they were before it.{others} The files as they \
                     were just before the rollback are saved in {pre} \
                     (`git restore --source={pre} --worktree -- .` brings them back)."
                )
            }
            _ => "No checkpoint was taken for it, so files were left as they are.".into(),
        };
        let item = self
            .new_item(
                thread_id,
                None,
                ItemKind::SystemNotice {
                    message: format!(
                        "Rolled back {turns} turn{}. {files}",
                        if turns == 1 { "" } else { "s" }
                    ),
                },
                "",
            )
            .map_err(|e| format!("{e:#}"))?;
        batch.events.push(EventKind::ItemAdded {
            item: Arc::new(item),
        });
        match live_rewind {
            Some(before_turn) => batch.effects.push(Effect::ProviderRewind {
                thread_id,
                before_turn,
            }),
            None => self.post.push(Post::Release(thread_id)),
        }
        Ok(())
    }

    fn live_thread(&self, thread_id: ThreadId) -> Result<&Thread, String> {
        match self.threads.get(&thread_id) {
            Some(t) if !t.archived => Ok(t),
            Some(_) => Err("thread is archived".into()),
            None => Err("unknown thread".into()),
        }
    }

    /// Mirror a committed approval answer into runtime state.
    fn after_commit_command(&mut self, row: &OutboxRow) {
        if let Effect::RuntimeRequestRespond {
            thread_id,
            provider_request_id,
            ..
        } = &row.effect
            && let Some(run) = self.active_run(*thread_id)
        {
            run.approvals.retain(|_, item| {
                !matches!(&item.kind, ItemKind::ApprovalRequest { provider_request_id: p, .. }
                    if p == provider_request_id)
            });
            if run.approvals.is_empty() && run.status == RunStatus::Waiting {
                run.status = RunStatus::Running;
            }
        }
    }

    // --------------------------------------------------------------- effects

    async fn run_effect(&mut self, row: OutboxRow) {
        self.after_commit_command(&row);
        let result = match row.effect.clone() {
            Effect::ProviderTurnStart {
                thread_id,
                run_id,
                message_id,
            } => self.start_turn(thread_id, run_id, message_id).await,
            Effect::ProviderInterrupt { thread_id, run_id } => {
                self.interrupt(thread_id, run_id).await;
                Ok(())
            }
            Effect::ProviderSteer {
                thread_id,
                run_id,
                message_id,
            } => self.steer(thread_id, run_id, message_id).await,
            Effect::ProviderRewind {
                thread_id,
                before_turn,
            } => {
                if let Some(live) = self.rt.get(&thread_id).and_then(|rt| rt.session.as_ref()) {
                    let _ = live.session.rewind(before_turn);
                }
                Ok(())
            }
            Effect::RuntimeRequestRespond {
                thread_id,
                provider_request_id,
                approve,
                ..
            } => {
                if let Some(live) = self.rt.get(&thread_id).and_then(|rt| rt.session.as_ref()) {
                    let decision = if approve {
                        HarnessDecision::Allow
                    } else {
                        HarnessDecision::Deny
                    };
                    let _ = live.session.approve(provider_request_id, decision);
                }
                Ok(())
            }
        };
        if let Err(err) = result {
            eprintln!("blongo-core: effect failed: {err:#}");
        }
        // Executed effects are deleted right away so the outbox only ever
        // holds work in flight.
        if let Err(err) = self.store.complete_effect(row.id) {
            eprintln!("blongo-core: outbox update failed: {err:#}");
        }
    }

    async fn start_turn(
        &mut self,
        thread_id: ThreadId,
        run_id: RunId,
        message_id: ItemId,
    ) -> anyhow::Result<()> {
        let rt = self.rt(thread_id)?;
        rt.idle_since = None;
        rt.run = Some(ActiveRun {
            run_id,
            status: RunStatus::Starting,
            open_text: None,
            tools: HashMap::new(),
            approvals: HashMap::new(),
            interrupt_deadline: None,
            plan: None,
            provider_turn_id: None,
            usage: None,
        });
        let thread = self
            .threads
            .get(&thread_id)
            .context("unknown thread")?
            .clone();
        if self.config.checkpoints
            && let Some(project) = self.projects.get(&thread.project_id)
        {
            // Capture the workspace before the run changes it, off the
            // loop; the thread's other work waits for it, the core does
            // not. Not being in a git repository is normal (no checkpoint).
            let cwd = PathBuf::from(thread.cwd(project));
            let name = blongo_git::checkpoint_ref(&thread_id.to_string(), &run_id.to_string());
            self.spawn_job(Key::Thread(thread_id), async move {
                let result = if blongo_git::work_tree_root(&cwd).await.is_none() {
                    Ok(None)
                } else {
                    blongo_git::capture_checkpoint(&cwd, &name)
                        .await
                        .map(Some)
                        .map_err(|e| format!("{e:#}"))
                };
                JobDone::Checkpoint {
                    thread_id,
                    run_id,
                    message_id,
                    result,
                }
            });
            return Ok(());
        }
        self.continue_turn(thread_id, run_id, message_id).await
    }

    /// Start (or reuse) the agent session and send the prompt.
    async fn continue_turn(
        &mut self,
        thread_id: ThreadId,
        run_id: RunId,
        message_id: ItemId,
    ) -> anyhow::Result<()> {
        let text = self
            .store
            .item(message_id)?
            .context("message not found")?
            .text;
        let thread = self
            .threads
            .get(&thread_id)
            .context("unknown thread")?
            .clone();
        let label = thread.provider.label();
        let fresh = !self
            .rt
            .get(&thread_id)
            .is_some_and(|rt| rt.session.is_some());
        if let Err(err) = self.ensure_session(thread_id).await {
            let message = format!("Could not start {label}: {err:#}");
            self.finish_run(thread_id, RunStatus::Failed, Some(message));
            return Ok(());
        }
        // A new provider conversation that must first learn this one.
        let text = if fresh && thread.pending_context == Some(PendingContext::Handoff) {
            self.handoff_prompt(thread_id, run_id, &text)?
        } else {
            text.to_string()
        };
        let sent = self
            .rt
            .get(&thread_id)
            .and_then(|rt| rt.session.as_ref())
            .map(|live| live.session.prompt(text));
        if !matches!(sent, Some(Ok(()))) {
            self.finish_run(
                thread_id,
                RunStatus::Failed,
                Some(format!("The {label} session ended unexpectedly.")),
            );
            return Ok(());
        }
        self.set_run_status(thread_id, RunStatus::Running);
        Ok(())
    }

    /// The prompt for the first turn of a provider conversation that takes
    /// over this thread: its visible messages so far (bounded, newest kept),
    /// then the new message.
    fn handoff_prompt(
        &self,
        thread_id: ThreadId,
        run_id: RunId,
        text: &str,
    ) -> anyhow::Result<String> {
        let items = self.store.items(thread_id)?;
        let mut entries: Vec<String> = items
            .iter()
            .filter(|i| i.run_id != Some(run_id))
            .filter_map(|i| match i.kind {
                ItemKind::UserMessage => Some(format!("User: {}", i.text)),
                ItemKind::AssistantMessage { .. } if !i.text.is_empty() => {
                    Some(format!("Assistant: {}", i.text))
                }
                _ => None,
            })
            .collect();
        if entries.is_empty() {
            return Ok(text.to_owned());
        }
        let mut total: usize = entries.iter().map(|e| e.len() + 2).sum();
        let mut omitted = 0;
        while total > MAX_HANDOFF_CHARS && entries.len() > 1 {
            total -= entries.remove(0).len() + 2;
            omitted += 1;
        }
        if total > MAX_HANDOFF_CHARS {
            let last = entries.last_mut().expect("one entry");
            let keep = last.len() - (total - MAX_HANDOFF_CHARS).min(last.len());
            let mut cut = last.len() - keep;
            while !last.is_char_boundary(cut) {
                cut += 1;
            }
            *last = format!("…{}", &last[cut..]);
        }
        let note = if omitted > 0 {
            format!("({omitted} earlier messages omitted)\n\n")
        } else {
            String::new()
        };
        Ok(format!(
            "<previous_conversation>\nThis conversation was started elsewhere (another \
             agent or session). The messages so far:\n\n{note}{}\n</previous_conversation>\n\n{text}",
            entries.join("\n\n")
        ))
    }

    async fn steer(
        &mut self,
        thread_id: ThreadId,
        run_id: RunId,
        message_id: ItemId,
    ) -> anyhow::Result<()> {
        let text = self
            .store
            .item(message_id)?
            .context("message not found")?
            .text;
        let still_running = self
            .active_run(thread_id)
            .is_some_and(|r| r.run_id == run_id);
        let live = self.rt.get(&thread_id).and_then(|rt| rt.session.as_ref());
        match live {
            Some(live) if still_running => {
                if live
                    .session
                    .steer(message_id.to_string(), text.to_string())
                    .is_err()
                {
                    // The session is gone: the steer never left.
                    self.requeue_steer(thread_id, message_id);
                }
            }
            _ => {
                // The turn ended before the steer got there.
                self.requeue_steer(thread_id, message_id);
            }
        }
        Ok(())
    }

    /// A steered message that never reached its turn becomes a queued run of
    /// its own (behind whatever runs now), so its text is neither lost nor
    /// sent as a turn the core does not know about.
    fn requeue_steer(&mut self, thread_id: ThreadId, message_id: ItemId) {
        let Ok(Some(item)) = self.store.item(message_id) else {
            return;
        };
        if item.thread_id != thread_id || item.kind != ItemKind::UserMessage {
            return;
        }
        if self.threads.get(&thread_id).is_none_or(|t| t.archived) {
            return;
        }
        // Already its own turn (moved before, then started): a duplicate.
        let heads_its_run = self.store.items(thread_id).is_ok_and(|items| {
            items
                .iter()
                .find(|i| i.run_id == item.run_id && i.kind == ItemKind::UserMessage)
                .is_some_and(|first| first.id == item.id)
        });
        if heads_its_run {
            return;
        }
        // Already moved (a duplicate report).
        if self
            .rt
            .get(&thread_id)
            .is_some_and(|rt| rt.queue.iter().any(|q| q.message_id == message_id))
        {
            return;
        }
        let Some(provider) = self.threads.get(&thread_id).map(|t| t.provider) else {
            return;
        };
        let run_id = RunId::new();
        let now = Timestamp::now();
        let mut events = self.close_text(thread_id);
        events.push(EventKind::RunCreated {
            run: Run::new(run_id, thread_id, RunStatus::Queued, provider, now),
        });
        events.push(EventKind::ItemUpdated {
            item: Arc::new(TurnItem {
                run_id: Some(run_id),
                ..item
            }),
        });
        if let Ok(notice) = self.new_item(
            thread_id,
            None,
            ItemKind::SystemNotice {
                message: "The turn ended before your message reached the agent; \
                          it was queued as the next turn."
                    .into(),
            },
            "",
        ) {
            events.push(EventKind::ItemAdded {
                item: Arc::new(notice),
            });
        }
        self.commit_events(events);
        if let Ok(rt) = self.rt(thread_id) {
            rt.queue.push_back(Queued { run_id, message_id });
        }
        self.ready.push(thread_id);
    }

    /// Start the next queued message of every thread whose run just ended.
    async fn start_queued(&mut self) {
        while let Some(thread_id) = self.ready.pop() {
            let next = match self.rt.get_mut(&thread_id) {
                // A job of the thread (a git commit) goes first; its end
                // makes the thread ready again.
                Some(rt) if rt.run.is_none() && !self.busy.contains(&thread_id) => {
                    rt.queue.pop_front()
                }
                _ => None,
            };
            let Some(queued) = next else {
                continue;
            };
            if self.live_thread(thread_id).is_err() {
                continue;
            }
            // The message moves to the end of the timeline, where its turn
            // now starts.
            let mut events = Vec::new();
            if let Ok(Some(item)) = self.store.item(queued.message_id)
                && let Ok(rt) = self.rt(thread_id)
            {
                let ordinal = rt.next_ordinal;
                rt.next_ordinal += 1;
                events.push(EventKind::ItemUpdated {
                    item: Arc::new(TurnItem { ordinal, ..item }),
                });
            }
            events.push(EventKind::RunStatusChanged {
                thread_id,
                run_id: queued.run_id,
                status: RunStatus::Starting,
                error: None,
            });
            self.commit_events(events);
            if let Err(err) = self
                .start_turn(thread_id, queued.run_id, queued.message_id)
                .await
            {
                eprintln!("blongo-core: queued turn failed to start: {err:#}");
            }
        }
    }

    fn session_config(
        &self,
        provider: ProviderKind,
        cwd: &str,
        model: Option<String>,
    ) -> SessionConfig {
        let mut config = SessionConfig::new(cwd);
        if let Some(exe) = self.config.executable(provider) {
            config = config.executable(exe);
        }
        for (k, v) in &self.config.agent_env {
            config = config.env(k, v);
        }
        config.model = model;
        config
    }

    async fn ensure_session(&mut self, thread_id: ThreadId) -> anyhow::Result<()> {
        if self
            .rt
            .get(&thread_id)
            .is_some_and(|rt| rt.session.is_some())
        {
            return Ok(());
        }
        let thread = self.threads.get(&thread_id).context("unknown thread")?;
        let project = self
            .projects
            .get(&thread.project_id)
            .context("unknown project")?;
        let provider = thread.provider;
        let mut config = self.session_config(provider, thread.cwd(project), thread.model.clone());
        let options = StartOptions {
            resume: thread.provider_thread_id.clone(),
            context: thread.pending_context.clone(),
            acp_args: self.config.acp_args.clone(),
        };
        // Blongo's own tools, as this thread.
        let grant = match (&self.mcp, self.mcp_bridge()) {
            (Some(server), Some((command, mut args))) => match server.register(thread_id) {
                Ok(grant) => {
                    args.push(server.socket.to_string_lossy().into_owned());
                    args.push(grant.file.to_string_lossy().into_owned());
                    config.mcp = Some(blongo_harness::McpServer {
                        name: "blongo".into(),
                        command,
                        args,
                    });
                    Some(grant)
                }
                Err(err) => {
                    eprintln!("blongo-core: no MCP access for this session: {err:#}");
                    None
                }
            },
            _ => None,
        };
        let mut session = blongo_harness::start(provider, config, options).await?;
        self.generation += 1;
        let generation = self.generation;
        // Forward this session's events into the core loop, tagged so a
        // released session's stragglers are ignored.
        let mut events = std::mem::replace(&mut session.events, mpsc::channel(1).1);
        let tx = self.session_tx.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let msg = SessionMsg {
                    thread_id,
                    generation,
                    event: Some(event),
                };
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
            let _ = tx
                .send(SessionMsg {
                    thread_id,
                    generation,
                    event: None,
                })
                .await;
        });
        self.rt(thread_id)?.session = Some(LiveSession {
            session,
            generation,
            provider,
            _mcp: grant,
            forwarder,
        });
        Ok(())
    }

    /// How agents start the MCP bridge (program, leading arguments).
    fn mcp_bridge(&self) -> Option<(PathBuf, Vec<String>)> {
        if let Some(bridge) = &self.config.mcp_bridge {
            return Some(bridge.clone());
        }
        std::env::current_exe()
            .ok()
            .map(|exe| (exe, vec!["mcp-bridge".to_owned()]))
    }

    // ------------------------------------------------------- sign-in, install

    fn login(&mut self, provider: ProviderKind) {
        let out = self.out.clone();
        if !provider.capabilities().interactive_login {
            let how = match provider {
                ProviderKind::Codex => "run `codex login` in a terminal",
                ProviderKind::ClaudeCode => "run `claude` in a terminal and use /login",
                ProviderKind::Antigravity => "sign in from Blongo",
                ProviderKind::Acp => "sign in with the agent's own command",
            };
            let _ = out.send(CoreEvent::Login {
                provider,
                state: LoginState::Failed(format!("To sign in to {}, {how}.", provider.label())),
            });
            return;
        }
        let cwd = dirs::home_dir().unwrap_or_else(|| self.config.data_dir.clone());
        let config = self.session_config(provider, &cwd.to_string_lossy(), None);
        let wait = self.config.login_timeout;
        tokio::spawn(async move {
            let (url_tx, mut url_rx) = mpsc::channel(4);
            let urls = {
                let out = out.clone();
                tokio::spawn(async move {
                    while let Some(url) = url_rx.recv().await {
                        let _ = out.send(CoreEvent::Login {
                            provider,
                            state: LoginState::Url(url),
                        });
                    }
                })
            };
            let result = acp::login(config, acp::antigravity(), url_tx, wait).await;
            let _ = urls.await;
            let state = match result {
                Ok(()) => LoginState::Succeeded,
                Err(err) => LoginState::Failed(format!("{err:#}")),
            };
            let _ = out.send(CoreEvent::Login { provider, state });
        });
    }

    /// Import t3code's history on its own database connection, off the
    /// loop (it can take a while); the loop keeps committing meanwhile
    /// (SQLite serializes the writes).
    fn import_t3(&mut self, source: PathBuf) {
        let busy: HashSet<ThreadId> = self
            .rt
            .iter()
            .filter(|(_, rt)| rt.run.is_some() || !rt.queue.is_empty())
            .map(|(id, _)| *id)
            .collect();
        let database = self.config.database.clone();
        self.spawn_job(Key::Global, async move {
            let result = tokio::task::spawn_blocking(move || {
                let mut store = Store::open(&database)?;
                crate::t3_import::import(&mut store, &source, &|id| busy.contains(&id))
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("import stopped: {e}")));
            JobDone::Imported(result)
        });
    }

    fn after_import(&mut self, result: anyhow::Result<ImportReport>) {
        if let Err(err) = self.store.refresh_last_sequence() {
            eprintln!("blongo-core: {err:#}");
        }
        // Threads that got turns the provider never saw start a fresh
        // provider conversation, and their cached ordinals are stale.
        if let Ok(report) = &result {
            for thread_id in &report.updated_threads {
                self.release_session(*thread_id);
                if self
                    .rt
                    .get(thread_id)
                    .is_some_and(|rt| rt.run.is_none() && rt.queue.is_empty())
                {
                    self.rt.remove(thread_id);
                }
            }
        }
        let reload = (|| -> anyhow::Result<()> {
            self.projects = self
                .store
                .projects()?
                .into_iter()
                .map(|p| (p.id, p))
                .collect();
            self.threads = self
                .store
                .threads(true)?
                .into_iter()
                .map(|t| (t.id, t))
                .collect();
            Ok(())
        })();
        let ok = result.is_ok();
        self.emit(CoreEvent::Imported(
            result
                .and_then(|r| reload.map(|()| r))
                .map_err(|e| format!("{e:#}")),
        ));
        if ok {
            match self.shell_snapshot() {
                Ok(shell) => self.emit(CoreEvent::Shell(Arc::new(shell))),
                Err(err) => eprintln!("blongo-core: shell snapshot failed: {err:#}"),
            }
        }
    }

    fn shell_snapshot(&self) -> anyhow::Result<ShellSnapshot> {
        Ok(ShellSnapshot {
            sequence: self.store.last_sequence(),
            projects: self.store.projects()?,
            threads: self.store.threads(false)?,
            schedules: self.store.schedules()?,
        })
    }

    fn install_antigravity(&mut self) {
        use std::sync::atomic::Ordering;
        if self.installing.swap(true, Ordering::AcqRel) {
            // A second click while installing: the running install reports.
            return;
        }
        let installing = self.installing.clone();
        let out = self.out.clone();
        let target = self.config.antigravity_install.clone();
        tokio::spawn(async move {
            struct Done(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            let _done = Done(installing);
            let fail = |message: String| {
                let _ = out.send(CoreEvent::Install(InstallState::Failed(message)));
            };
            let Some(pin) = target.pin.or_else(antigravity_install::antigravity_pin) else {
                return fail("Antigravity has no build for this platform.".into());
            };
            let Some(root) = target.root.or_else(antigravity_install::default_root) else {
                return fail("No data directory to install into (HOME is unset).".into());
            };
            let progress = {
                let out = out.clone();
                move |message: String| {
                    let _ = out.send(CoreEvent::Install(InstallState::Progress(message)));
                }
            };
            match antigravity_install::install(&root, &pin, &target.origin, progress).await {
                Ok(entry) => {
                    let _ = out.send(CoreEvent::Install(InstallState::Done(entry)));
                }
                Err(err) => fail(format!("{err:#}")),
            }
        });
    }

    async fn interrupt(&mut self, thread_id: ThreadId, run_id: RunId) {
        let timeout = self.config.interrupt_timeout;
        let Some(rt) = self.rt.get_mut(&thread_id) else {
            return;
        };
        let Some(run) = rt.run.as_mut().filter(|r| r.run_id == run_id) else {
            return;
        };
        match &rt.session {
            Some(live) if live.session.interrupt().is_ok() => {
                run.interrupt_deadline
                    .get_or_insert_with(|| Instant::now() + timeout);
            }
            _ => self.finish_run(thread_id, RunStatus::Interrupted, None),
        }
    }

    // -------------------------------------------------------- provider events

    async fn on_session_msg(&mut self, msg: SessionMsg) {
        let current = self
            .rt
            .get(&msg.thread_id)
            .and_then(|rt| rt.session.as_ref())
            .is_some_and(|live| live.generation == msg.generation);
        if !current {
            // A released session can still report a steer it never
            // delivered: its text must not be lost.
            if let Some(AgentEvent::SteerNotDelivered { id }) = &msg.event
                && let Some(message_id) = ItemId::parse(id)
            {
                self.requeue_steer(msg.thread_id, message_id);
            }
            return;
        }
        match msg.event {
            Some(event) => self.on_agent_event(msg.thread_id, event),
            None => {
                let rt = self.rt.get_mut(&msg.thread_id).expect("checked");
                let session = rt.session.take();
                let label = session
                    .as_ref()
                    .map_or("agent", |live| live.provider.label());
                match rt.run.as_ref().map(|r| r.interrupt_deadline.is_some()) {
                    // Exiting is one way to honour an interrupt.
                    Some(true) => self.finish_run(msg.thread_id, RunStatus::Interrupted, None),
                    Some(false) => self.finish_run(
                        msg.thread_id,
                        RunStatus::Failed,
                        Some(format!("The {label} process exited.")),
                    ),
                    None if rt.queue.is_empty() => {
                        self.rt.remove(&msg.thread_id);
                    }
                    None => {}
                }
                if let Some(live) = session {
                    live.release();
                }
            }
        }
    }

    fn on_agent_event(&mut self, thread_id: ThreadId, event: AgentEvent) {
        if let AgentEvent::SteerNotDelivered { id } = &event {
            // May come before or after the turn's end: never tied to a run.
            if let Some(message_id) = ItemId::parse(id) {
                self.requeue_steer(thread_id, message_id);
            }
            return;
        }
        if let AgentEvent::Models { models } = event {
            if let Some(thread) = self.threads.get(&thread_id) {
                self.emit(CoreEvent::Models {
                    provider: thread.provider,
                    models: models.into(),
                });
            }
            return;
        }
        if let AgentEvent::SessionStarted {
            provider_session_id,
        } = event
        {
            let Some(thread) = self.threads.get(&thread_id) else {
                return;
            };
            let previous = thread.provider_thread_id.clone();
            let expected_new = thread.pending_context.is_some();
            let label = thread.provider.label();
            if previous.as_deref() != Some(provider_session_id.as_str()) || expected_new {
                let mut events = vec![EventKind::ThreadProviderBound {
                    thread_id,
                    provider_thread_id: provider_session_id.clone(),
                }];
                if previous.is_some()
                    && !expected_new
                    && previous.as_deref() != Some(provider_session_id.as_str())
                    && let Ok(item) = self.new_item(
                        thread_id,
                        self.rt
                            .get(&thread_id)
                            .and_then(|rt| rt.run.as_ref())
                            .map(|r| r.run_id),
                        ItemKind::SystemNotice {
                            message: format!(
                                "{label} could not resume the previous conversation; \
                                 this turn starts without its context."
                            ),
                        },
                        "",
                    )
                {
                    events.push(EventKind::ItemAdded {
                        item: Arc::new(item),
                    });
                }
                self.commit_events(events);
            }
            return;
        }
        let Some(run_id) = self.active_run(thread_id).map(|r| r.run_id) else {
            // Output outside a run (e.g. a late error after interrupt).
            if let AgentEvent::Error { message } = event {
                eprintln!("blongo-core: agent error outside a turn: {message}");
            }
            return;
        };
        match event {
            AgentEvent::SessionStarted { .. }
            | AgentEvent::Models { .. }
            | AgentEvent::SteerNotDelivered { .. } => unreachable!(),
            AgentEvent::ProviderTurnId { id } => {
                let Some(run) = self.active_run(thread_id) else {
                    return;
                };
                if run.provider_turn_id.as_deref() != Some(id.as_str()) {
                    run.provider_turn_id = Some(id.clone());
                    self.commit_events(vec![EventKind::RunProviderTurn {
                        thread_id,
                        run_id,
                        provider_turn_id: id,
                    }]);
                }
            }
            AgentEvent::Plan { steps } => self.on_plan(thread_id, run_id, steps),
            AgentEvent::Usage(usage) => {
                let Some(run) = self.active_run(thread_id) else {
                    return;
                };
                if run.usage != Some(usage) {
                    run.usage = Some(usage);
                    self.commit_events(vec![EventKind::RunUsage {
                        thread_id,
                        run_id,
                        usage,
                    }]);
                }
            }
            AgentEvent::TextDelta { text } => self.on_text(thread_id, TextKind::Assistant, text),
            AgentEvent::ReasoningDelta { text } => {
                self.on_text(thread_id, TextKind::Reasoning, text)
            }
            AgentEvent::ToolCall {
                call_id,
                name,
                input,
            } => {
                let mut events = self.close_text(thread_id);
                let kind = tool_kind(&call_id, &name, &input);
                if let Ok(item) = self.new_item(thread_id, Some(run_id), kind, "") {
                    let item = Arc::new(item);
                    if let Some(run) = self.active_run(thread_id) {
                        run.tools.insert(call_id, item.clone());
                    }
                    events.push(EventKind::ItemAdded { item });
                }
                self.commit_events(events);
            }
            AgentEvent::ToolResult {
                call_id,
                is_error,
                output,
                exit_code,
            } => {
                let mut events = self.close_text(thread_id);
                let Some(run) = self.active_run(thread_id) else {
                    return;
                };
                if let Some(item) = run.tools.get(&call_id) {
                    let updated = Arc::new(with_tool_result(item, is_error, &output, exit_code));
                    run.tools.insert(call_id, updated.clone());
                    events.push(EventKind::ItemUpdated { item: updated });
                }
                self.commit_events(events);
            }
            AgentEvent::ApprovalRequest {
                request_id,
                title,
                detail,
            } => {
                let mut events = self.close_text(thread_id);
                if self.config.settings.approval == ApprovalPolicy::AutoApprove {
                    // Recorded as approved; the run never waits.
                    let kind = ItemKind::ApprovalRequest {
                        provider_request_id: request_id.clone(),
                        title,
                        detail,
                        state: ApprovalState::Approved,
                    };
                    if let Ok(item) = self.new_item(thread_id, Some(run_id), kind, "") {
                        events.push(EventKind::ItemAdded {
                            item: Arc::new(item),
                        });
                    }
                    self.commit_events(events);
                    if let Some(live) = self.rt.get(&thread_id).and_then(|rt| rt.session.as_ref()) {
                        let _ = live.session.approve(request_id, HarnessDecision::Allow);
                    }
                    return;
                }
                let kind = ItemKind::ApprovalRequest {
                    provider_request_id: request_id,
                    title,
                    detail,
                    state: ApprovalState::Pending,
                };
                if let Ok(item) = self.new_item(thread_id, Some(run_id), kind, "") {
                    let item = Arc::new(item);
                    events.push(EventKind::ItemAdded { item: item.clone() });
                    if let Some(run) = self.active_run(thread_id) {
                        run.approvals.insert(item.id, item);
                        if run.status != RunStatus::Waiting {
                            run.status = RunStatus::Waiting;
                            events.push(EventKind::RunStatusChanged {
                                thread_id,
                                run_id,
                                status: RunStatus::Waiting,
                                error: None,
                            });
                        }
                    }
                }
                self.commit_events(events);
            }
            AgentEvent::AuthRequired { message, url } => {
                let message = match url {
                    Some(url) => format!("{message}\n{url}"),
                    None => message,
                };
                self.add_item(thread_id, run_id, ItemKind::SystemNotice { message });
            }
            AgentEvent::Error { message } => {
                self.add_item(thread_id, run_id, ItemKind::Error { message });
            }
            AgentEvent::TurnCompleted { status } => {
                let status = match status {
                    TurnStatus::Completed => RunStatus::Completed,
                    TurnStatus::Interrupted => RunStatus::Interrupted,
                    TurnStatus::Failed => RunStatus::Failed,
                };
                self.finish_run(thread_id, status, None);
            }
        }
    }

    /// The run's plan: one item, replaced in place on every update.
    fn on_plan(&mut self, thread_id: ThreadId, run_id: RunId, steps: Vec<PlanStep>) {
        let existing = self.active_run(thread_id).and_then(|r| r.plan.clone());
        let (event, item) = match existing {
            Some(item) => {
                let mut updated = (*item).clone();
                updated.kind = ItemKind::Plan { steps };
                let updated = Arc::new(updated);
                (
                    EventKind::ItemUpdated {
                        item: updated.clone(),
                    },
                    updated,
                )
            }
            None => {
                let mut events = self.close_text(thread_id);
                let Ok(item) = self.new_item(thread_id, Some(run_id), ItemKind::Plan { steps }, "")
                else {
                    return;
                };
                let item = Arc::new(item);
                events.push(EventKind::ItemAdded { item: item.clone() });
                self.commit_events(events);
                if let Some(run) = self.active_run(thread_id) {
                    run.plan = Some(item);
                }
                return;
            }
        };
        if let Some(run) = self.active_run(thread_id) {
            run.plan = Some(item);
        }
        self.commit_events(vec![event]);
    }

    fn add_item(&mut self, thread_id: ThreadId, run_id: RunId, kind: ItemKind) {
        let mut events = self.close_text(thread_id);
        if let Ok(item) = self.new_item(thread_id, Some(run_id), kind, "") {
            events.push(EventKind::ItemAdded {
                item: Arc::new(item),
            });
        }
        self.commit_events(events);
    }

    fn on_text(&mut self, thread_id: ThreadId, kind: TextKind, text: String) {
        if text.is_empty() {
            return;
        }
        let same = self
            .active_run(thread_id)
            .and_then(|r| r.open_text.as_ref())
            .is_some_and(|open| open.kind == kind);
        if !same {
            let mut events = self.close_text(thread_id);
            let run_id = self.active_run(thread_id).map(|r| r.run_id);
            let item_kind = match kind {
                TextKind::Assistant => ItemKind::AssistantMessage { streaming: true },
                TextKind::Reasoning => ItemKind::Reasoning { streaming: true },
            };
            let Ok(item) = self.new_item(thread_id, run_id, item_kind, "") else {
                return;
            };
            let item_id = item.id;
            events.push(EventKind::ItemAdded {
                item: Arc::new(item),
            });
            self.commit_events(events);
            if let Some(run) = self.active_run(thread_id) {
                run.open_text = Some(OpenText {
                    item_id,
                    kind,
                    pending: String::new(),
                    len: 0,
                });
            }
        }
        let flush_after = self.config.text_flush_interval;
        let Some(open) = self
            .active_run(thread_id)
            .and_then(|r| r.open_text.as_mut())
        else {
            return;
        };
        open.pending.push_str(&text);
        let item_id = open.item_id;
        self.emit(CoreEvent::TextDelta {
            thread_id,
            item_id,
            chunk: text.into(),
        });
        self.flush_deadline
            .get_or_insert_with(|| Instant::now() + flush_after);
    }

    /// Events that persist pending text of the thread's open text item and
    /// mark it finished. The item is closed in runtime state.
    fn close_text(&mut self, thread_id: ThreadId) -> Vec<EventKind> {
        let Some(open) = self.active_run(thread_id).and_then(|r| r.open_text.take()) else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(2);
        if !open.pending.is_empty() {
            events.push(EventKind::ItemTextAppended {
                thread_id,
                item_id: open.item_id,
                len: open.len + open.pending.len() as u64,
                chunk: open.pending.into(),
            });
        }
        events.push(EventKind::ItemFinished {
            thread_id,
            item_id: open.item_id,
        });
        events
    }

    /// Commit every thread's pending streamed text (the coalesced write).
    fn flush_text(&mut self, only: Option<ThreadId>) {
        let mut events = Vec::new();
        for (thread_id, rt) in &mut self.rt {
            if only.is_some_and(|t| t != *thread_id) {
                continue;
            }
            if let Some(open) = rt.run.as_mut().and_then(|r| r.open_text.as_mut())
                && !open.pending.is_empty()
            {
                let chunk = std::mem::take(&mut open.pending);
                open.len += chunk.len() as u64;
                events.push(EventKind::ItemTextAppended {
                    thread_id: *thread_id,
                    item_id: open.item_id,
                    len: open.len,
                    chunk: chunk.into(),
                });
            }
        }
        if only.is_none() {
            self.flush_deadline = None;
        }
        self.commit_events(events);
    }

    fn set_run_status(&mut self, thread_id: ThreadId, status: RunStatus) {
        let Some(run) = self.active_run(thread_id) else {
            return;
        };
        if run.status == status {
            return;
        }
        // An approval may already be waiting by the time the prompt is sent.
        if status == RunStatus::Running && run.status == RunStatus::Waiting {
            return;
        }
        run.status = status;
        let run_id = run.run_id;
        self.commit_events(vec![EventKind::RunStatusChanged {
            thread_id,
            run_id,
            status,
            error: None,
        }]);
    }

    /// End the thread's root run: persist text, close open items, record the
    /// final status. This is the only place a turn completes.
    fn finish_run(&mut self, thread_id: ThreadId, status: RunStatus, error: Option<String>) {
        let mut events = self.close_text(thread_id);
        let Some(run) = self.rt.get_mut(&thread_id).and_then(|rt| rt.run.take()) else {
            return;
        };
        for item in run.approvals.values() {
            events.push(EventKind::ItemUpdated {
                item: Arc::new(with_approval_state(item, ApprovalState::Cancelled)),
            });
        }
        for item in run.tools.values() {
            if let Some(closed) = close_tool(item) {
                events.push(EventKind::ItemUpdated {
                    item: Arc::new(closed),
                });
            }
        }
        if let Some(message) = &error
            && let Ok(item) = self.new_item(
                thread_id,
                Some(run.run_id),
                ItemKind::Error {
                    message: message.clone(),
                },
                "",
            )
        {
            events.push(EventKind::ItemAdded {
                item: Arc::new(item),
            });
        }
        if status == RunStatus::Interrupted
            && let Ok(item) = self.new_item(
                thread_id,
                Some(run.run_id),
                ItemKind::SystemNotice {
                    message: "Turn interrupted.".into(),
                },
                "",
            )
        {
            events.push(EventKind::ItemAdded {
                item: Arc::new(item),
            });
        }
        events.push(EventKind::RunStatusChanged {
            thread_id,
            run_id: run.run_id,
            status,
            error,
        });
        self.commit_events(events);
        match self.rt.get_mut(&thread_id) {
            Some(rt) if !rt.queue.is_empty() => {
                rt.idle_since = Some(Instant::now());
                self.ready.push(thread_id);
            }
            Some(rt) if rt.session.is_some() => rt.idle_since = Some(Instant::now()),
            // No process to keep warm: drop the runtime state entirely.
            Some(_) => {
                self.rt.remove(&thread_id);
            }
            None => {}
        }
        eprintln!("blongo: run {} finished ({status:?})", run.run_id);
        self.emit(CoreEvent::RunFinished { thread_id, status });
        self.forge_after_turn(thread_id);
        if !self.is_busy(thread_id) {
            self.resolve_waiters(thread_id);
        }
    }

    // ---------------------------------------------------------------- timers

    fn next_timer(&self) -> Option<Instant> {
        let mut next = self.flush_deadline;
        let mut consider = |t: Instant| {
            next = Some(next.map_or(t, |n: Instant| n.min(t)));
        };
        if let Some(due) = self
            .schedules
            .values()
            .filter(|s| s.enabled)
            .filter_map(|s| s.next_run_at)
            .min()
        {
            // Sleep until it is due, but re-check the wall clock at least
            // every 15 minutes: the monotonic clock stops during suspend
            // and does not follow clock changes.
            let ms = (due.0 - Timestamp::now().0).clamp(0, 15 * 60_000) as u64;
            consider(Instant::now() + Duration::from_millis(ms));
        }
        if let Some(due) = self.forge.next_due() {
            consider(due);
        }
        for rt in self.rt.values() {
            if let Some(deadline) = rt.run.as_ref().and_then(|r| r.interrupt_deadline) {
                consider(deadline);
            }
            if rt.session.is_some()
                && rt.run.is_none()
                && let Some(idle) = rt.idle_since
            {
                consider(idle + self.config.session_idle_timeout);
            }
        }
        next
    }

    async fn on_timer(&mut self) {
        let wall = Timestamp::now();
        let due: Vec<ScheduleId> = self
            .schedules
            .values()
            .filter(|s| s.enabled && s.next_run_at.is_some_and(|t| t <= wall))
            .map(|s| s.id)
            .collect();
        for id in due {
            // Missed while Blongo was not running: runs once now.
            self.fire_schedule(id, true);
        }
        let now = Instant::now();
        if self.flush_deadline.is_some_and(|d| d <= now) {
            self.flush_text(None);
        }
        self.forge_tick();
        let overdue: Vec<ThreadId> = self
            .rt
            .iter()
            .filter(|(_, rt)| {
                rt.run
                    .as_ref()
                    .and_then(|r| r.interrupt_deadline)
                    .is_some_and(|d| d <= now)
            })
            .map(|(id, _)| *id)
            .collect();
        for thread_id in overdue {
            // The agent ignored the interrupt: stop it.
            let label = self
                .threads
                .get(&thread_id)
                .map_or("The agent", |t| t.provider.label());
            self.release_session(thread_id);
            self.finish_run(
                thread_id,
                RunStatus::Interrupted,
                Some(format!(
                    "{label} did not stop in time; its process was terminated."
                )),
            );
        }
        let idle_timeout = self.config.session_idle_timeout;
        let idle: Vec<ThreadId> = self
            .rt
            .iter()
            .filter(|(_, rt)| {
                rt.session.is_some()
                    && rt.run.is_none()
                    && rt.idle_since.is_some_and(|t| t + idle_timeout <= now)
            })
            .map(|(id, _)| *id)
            .collect();
        for thread_id in idle {
            self.release_session(thread_id);
            // Nothing left worth keeping in memory for this thread.
            if self
                .rt
                .get(&thread_id)
                .is_some_and(|rt| rt.queue.is_empty())
            {
                self.rt.remove(&thread_id);
            }
        }
    }

    fn release_session(&mut self, thread_id: ThreadId) {
        if let Some(live) = self.rt.get_mut(&thread_id).and_then(|rt| rt.session.take()) {
            live.release();
        }
    }

    // ------------------------------------------------------------- snapshots

    fn open_thread(&mut self, thread_id: ThreadId) {
        // Persist pending text so the snapshot body is complete; deltas
        // after this point follow the snapshot in the channel.
        self.flush_text(Some(thread_id));
        let snapshot = (|| -> anyhow::Result<ThreadSnapshot> {
            Ok(ThreadSnapshot {
                thread_id,
                sequence: self.store.last_sequence(),
                runs: self.store.runs(thread_id)?,
                items: self.store.items(thread_id)?,
            })
        })();
        match snapshot {
            Ok(snapshot) => self.emit(CoreEvent::Thread(Arc::new(snapshot))),
            Err(err) => eprintln!("blongo-core: snapshot of {thread_id} failed: {err:#}"),
        }
    }

    // ------------------------------------------------------------- lifecycle

    async fn shutdown(&mut self) {
        let running: Vec<ThreadId> = self
            .rt
            .iter()
            .filter(|(_, rt)| rt.run.is_some())
            .map(|(id, _)| *id)
            .collect();
        for thread_id in running {
            self.finish_run(
                thread_id,
                RunStatus::Interrupted,
                Some("Blongo quit while this turn was running.".into()),
            );
        }
        self.flush_text(None);
        self.stop_sessions().await;
    }

    async fn abort(&mut self) {
        self.stop_sessions().await;
    }

    async fn stop_sessions(&mut self) {
        let sessions: Vec<LiveSession> = self
            .rt
            .values_mut()
            .filter_map(|rt| rt.session.take())
            .collect();
        // Forwarders first: once they are gone the drivers' event sends fail
        // and each driver goes straight to closing stdin and reaping.
        let mut shutdowns = Vec::with_capacity(sessions.len());
        for live in sessions {
            live.forwarder.abort();
            let pid = live.session.pid();
            shutdowns.push((pid, tokio::spawn(live.session.shutdown())));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for (_, handle) in &mut shutdowns {
            if tokio::time::timeout_at(deadline, &mut *handle)
                .await
                .is_err()
            {
                break;
            }
        }
        // Do not leave an agent (or its tool processes) behind. A finished
        // shutdown task has reaped its child, so its pid (and group id) may
        // already belong to an unrelated process: never signal those.
        for (pid, handle) in shutdowns {
            if !handle.is_finished()
                && let Some(pid) = pid
            {
                blongo_harness::process::kill_group(pid);
            }
        }
    }
}

/// The scheduling key of a command.
fn command_key(command: &Command) -> Key {
    match command {
        Command::ThreadRollback { .. } => Key::Global,
        Command::ProjectCreate { .. }
        | Command::ProjectSetForge { .. }
        | Command::ScheduleCreate { .. }
        | Command::ScheduleUpdate { .. }
        | Command::ScheduleDelete { .. }
        | Command::ScheduleRunNow { .. } => Key::None,
        other => other.thread_id().map_or(Key::None, Key::Thread),
    }
}

/// A command's I/O, off the loop.
async fn run_prep(plan: PrepPlan) -> Result<Prepared, String> {
    match plan {
        PrepPlan::Worktree {
            project_path,
            path,
            branch,
            base,
        } => {
            let root = blongo_git::work_tree_root(&project_path)
                .await
                .ok_or_else(|| format!("{} is not in a git repository", project_path.display()))?;
            let start = match base {
                Some(plan) => forge::worktree_base(&root, plan).await,
                None => forge::WorktreeBase::default(),
            };
            blongo_git::add_worktree(&root, &path, &branch, start.start.as_deref())
                .await
                .map_err(|e| format!("{e:#}"))?;
            // A project inside a larger repository keeps its relative place
            // in the worktree.
            let rel = project_path
                .canonicalize()
                .ok()
                .zip(root.canonicalize().ok())
                .and_then(|(p, r)| p.strip_prefix(r).ok().map(Path::to_path_buf))
                .unwrap_or_default();
            Ok(Prepared {
                worktree: Some(Worktree {
                    path: path.join(rel).to_string_lossy().into_owned(),
                    branch,
                }),
                notice: start.notice,
                base: start.base,
                ..Prepared::default()
            })
        }
        PrepPlan::Rollback {
            cwd,
            pre,
            commit,
            sharers,
        } => {
            // Keep what is about to be overwritten.
            blongo_git::capture_checkpoint(&cwd, &pre)
                .await
                .map_err(|e| {
                    format!("could not save the current files before rolling back: {e:#}")
                })?;
            blongo_git::restore_checkpoint(&cwd, &commit)
                .await
                .map_err(|e| {
                    format!(
                        "could not restore the files: {e:#}. The files may be partly restored; \
                         how they were before is saved in {pre} \
                         (`git restore --source={pre} --worktree -- .`)"
                    )
                })?;
            Ok(Prepared {
                worktree: None,
                restored: Some(commit),
                pre_rollback: Some(pre),
                sharers,
                ..Prepared::default()
            })
        }
        PrepPlan::LinkPr {
            ctx,
            cwd,
            reference,
        } => Ok(Prepared {
            pr: Some(Box::new(forge::link(ctx, cwd, reference).await?)),
            ..Prepared::default()
        }),
    }
}

fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    Path::new(path).to_path_buf()
}

fn title_from(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() <= MAX_TITLE {
        return line.to_owned();
    }
    let mut title: String = line.chars().take(MAX_TITLE - 1).collect();
    title.push('…');
    title
}

fn preview(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_PREVIEW {
        return text.to_owned();
    }
    let mut end = MAX_OUTPUT_PREVIEW;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn tool_kind(call_id: &str, name: &str, input: &Value) -> ItemKind {
    match name {
        "shell" => {
            let command = match input.get("command") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            ItemKind::CommandExecution {
                call_id: call_id.into(),
                command,
                status: ToolStatus::Running,
                output: String::new(),
                exit_code: None,
            }
        }
        "apply_patch" => {
            let paths = match input.get("changes") {
                Some(Value::Array(changes)) => changes
                    .iter()
                    .filter_map(|c| c.get("path").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect(),
                Some(Value::Object(map)) => map.keys().cloned().collect(),
                _ => Vec::new(),
            };
            ItemKind::FileChange {
                call_id: call_id.into(),
                paths,
                status: ToolStatus::Running,
            }
        }
        _ => ItemKind::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: preview(&match input {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
            status: ToolStatus::Running,
            output: String::new(),
        },
    }
}

fn with_tool_result(item: &TurnItem, is_error: bool, output: &str, exit: Option<i32>) -> TurnItem {
    let mut item = item.clone();
    let status = if is_error {
        ToolStatus::Failed
    } else {
        ToolStatus::Completed
    };
    match &mut item.kind {
        ItemKind::CommandExecution {
            status: s,
            output: o,
            exit_code,
            ..
        } => {
            *s = status;
            *o = preview(output);
            *exit_code = exit;
        }
        ItemKind::ToolCall {
            status: s,
            output: o,
            ..
        } => {
            *s = status;
            *o = preview(output);
        }
        ItemKind::FileChange { status: s, .. } => {
            *s = if output == "declined" {
                ToolStatus::Declined
            } else {
                status
            }
        }
        _ => {}
    }
    item
}

/// A still-running tool item marked failed (its run ended).
fn close_tool(item: &TurnItem) -> Option<TurnItem> {
    let mut item = item.clone();
    match &mut item.kind {
        ItemKind::CommandExecution { status, .. }
        | ItemKind::FileChange { status, .. }
        | ItemKind::ToolCall { status, .. }
            if *status == ToolStatus::Running =>
        {
            *status = ToolStatus::Failed;
            Some(item)
        }
        _ => None,
    }
}

fn with_approval_state(item: &TurnItem, new_state: ApprovalState) -> TurnItem {
    let mut item = item.clone();
    if let ItemKind::ApprovalRequest { state, .. } = &mut item.kind {
        *state = new_state;
    }
    item
}

/// The event that closes an item left open by a dead process (recovery).
fn close_item_event(item: &TurnItem) -> Option<EventKind> {
    match &item.kind {
        ItemKind::AssistantMessage { streaming: true }
        | ItemKind::Reasoning { streaming: true } => Some(EventKind::ItemFinished {
            thread_id: item.thread_id,
            item_id: item.id,
        }),
        ItemKind::ApprovalRequest {
            state: ApprovalState::Pending,
            ..
        } => Some(EventKind::ItemUpdated {
            item: Arc::new(with_approval_state(item, ApprovalState::Cancelled)),
        }),
        _ => close_tool(item).map(|item| EventKind::ItemUpdated {
            item: Arc::new(item),
        }),
    }
}

fn canonical(path: &str) -> PathBuf {
    Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
}

fn fork_title(title: &str) -> String {
    let base = title.strip_suffix(" (fork)").unwrap_or(title);
    format!("{base} (fork)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_first_line_and_bounded() {
        assert_eq!(title_from("\n  fix the build \nmore"), "fix the build");
        let long = "x".repeat(100);
        let title = title_from(&long);
        assert_eq!(title.chars().count(), MAX_TITLE);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn tool_kinds_from_codex_items() {
        let kind = tool_kind(
            "c1",
            "shell",
            &serde_json::json!({"command": ["bash", "-lc", "ls"]}),
        );
        assert!(
            matches!(kind, ItemKind::CommandExecution { ref command, .. } if command == "bash -lc ls")
        );
        let kind = tool_kind(
            "f1",
            "apply_patch",
            &serde_json::json!({"changes": [{"path": "a.rs"}, {"path": "b.rs"}]}),
        );
        assert!(
            matches!(kind, ItemKind::FileChange { ref paths, .. } if paths == &["a.rs", "b.rs"])
        );
        let kind = tool_kind("m1", "fs/read", &serde_json::json!({"p": 1}));
        assert!(matches!(kind, ItemKind::ToolCall { ref input, .. } if input == "{\"p\":1}"));
    }
}
