//! The core task: command handling, provider event translation, coalesced
//! text persistence, effects, recovery and session lifecycle.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use blongo_harness::{ApprovalDecision as HarnessDecision, Session, SessionConfig, codex};
use blongo_protocol::{
    AgentEvent, ApprovalDecision, ApprovalState, Command, CommandEnvelope, EventKind, ItemId,
    ItemKind, Project, ProjectId, Run, RunId, RunStatus, ShellSnapshot, Thread, ThreadId,
    ThreadSnapshot, ThreadStatus, Timestamp, ToolStatus, TurnItem, TurnStatus,
};
use blongo_store::{Batch, CommitOutcome, Effect, EffectStatus, OutboxRow, Store};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::{CoreConfig, CoreEvent, Request};

/// Default title of a new thread; replaced by the first message.
pub const DEFAULT_TITLE: &str = "New thread";
const MAX_OUTPUT_PREVIEW: usize = 4 * 1024;
const MAX_TITLE: usize = 48;

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
}

/// Runtime state for a thread that has (or had) an agent session or run.
struct ThreadRt {
    next_ordinal: u32,
    session: Option<LiveSession>,
    run: Option<ActiveRun>,
    idle_since: Option<Instant>,
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
}

pub(crate) async fn run(
    store: Store,
    config: CoreConfig,
    mut requests: mpsc::UnboundedReceiver<Request>,
    out: mpsc::UnboundedSender<CoreEvent>,
) {
    // Bounded: a core busy committing back-pressures the agents' stdout
    // instead of queueing their output in memory.
    let (session_tx, mut session_rx) = mpsc::channel(SESSION_CHANNEL_CAPACITY);
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
    };
    if let Err(err) = core.start() {
        eprintln!("blongo-core: startup failed: {err:#}");
        return;
    }
    loop {
        let timer = core.next_timer();
        tokio::select! {
            request = requests.recv() => match request {
                Some(Request::Dispatch(command)) => core.dispatch(command).await,
                Some(Request::OpenThread(thread_id)) => core.open_thread(thread_id),
                Some(Request::Abort) => {
                    core.abort().await;
                    return;
                }
                Some(Request::Shutdown) | None => {
                    core.shutdown().await;
                    return;
                }
            },
            Some(msg) = session_rx.recv() => core.on_session_msg(msg).await,
            _ = sleep_until(timer), if timer.is_some() => core.on_timer().await,
        }
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
        let snapshot = ShellSnapshot {
            sequence: self.store.last_sequence(),
            projects: self.store.projects()?,
            threads: self.store.threads(false)?,
        };
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
            events.push(EventKind::ItemAdded {
                item: Arc::new(TurnItem {
                    id: ItemId::new(),
                    thread_id: run.thread_id,
                    run_id: Some(run.id),
                    ordinal,
                    created_at: Timestamp::now(),
                    kind: ItemKind::SystemNotice {
                        message: "Interrupted: Blongo exited before this turn finished.".into(),
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
                self.threads.insert(thread.id, thread.clone());
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
                }
            }
            EventKind::RunStatusChanged {
                thread_id, status, ..
            } => {
                if let Some(t) = self.threads.get_mut(thread_id) {
                    t.status = status.thread_status();
                }
            }
            EventKind::RunCreated { run } => {
                if let Some(t) = self.threads.get_mut(&run.thread_id) {
                    t.status = run.status.thread_status();
                }
            }
            _ => {}
        }
    }

    fn reject(&self, command: &CommandEnvelope, reason: impl Into<String>) {
        self.emit(CoreEvent::CommandRejected {
            command_id: command.command_id,
            reason: reason.into(),
        });
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

    // -------------------------------------------------------------- commands

    async fn dispatch(&mut self, command: CommandEnvelope) {
        match self.store.receipt(command.command_id) {
            Ok(Some(_)) => {
                self.emit(CoreEvent::CommandDuplicate {
                    command_id: command.command_id,
                });
                return;
            }
            Ok(None) => {}
            Err(err) => return self.reject(&command, format!("{err:#}")),
        }
        let batch = match self.decide(&command) {
            Ok(batch) => batch,
            Err(reason) => return self.reject(&command, reason),
        };
        match self.commit(batch) {
            Ok(outbox) => {
                for row in outbox {
                    self.run_effect(row).await;
                }
            }
            Err(err) => {
                // Ordinals handed out for the failed batch are not reused,
                // which is harmless (ordinals only need to be increasing).
                self.reject(&command, format!("{err:#}"));
            }
        }
    }

    /// Validate a command and decide its events and effects (no side
    /// effects besides reading state).
    fn decide(&mut self, envelope: &CommandEnvelope) -> Result<Batch, String> {
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
                    },
                });
            }
            Command::ThreadCreate {
                thread_id,
                project_id,
                title,
            } => {
                if !self.projects.contains_key(project_id) {
                    return Err("unknown project".into());
                }
                if self.threads.contains_key(thread_id) {
                    return Err("thread already exists".into());
                }
                let title = title.trim();
                batch.events.push(EventKind::ThreadCreated {
                    thread: Thread {
                        id: *thread_id,
                        project_id: *project_id,
                        title: if title.is_empty() {
                            DEFAULT_TITLE.into()
                        } else {
                            title.into()
                        },
                        status: ThreadStatus::Idle,
                        archived: false,
                        created_at: now,
                        updated_at: now,
                        provider_thread_id: None,
                    },
                });
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
                self.live_thread(*thread_id)?;
                if self.active_run(*thread_id).is_some() {
                    return Err("stop the running turn first".into());
                }
                batch.events.push(EventKind::ThreadArchived {
                    thread_id: *thread_id,
                });
            }
            Command::MessageDispatch {
                thread_id,
                message_id,
                run_id,
                text,
            } => {
                let thread = self.live_thread(*thread_id)?.clone();
                if text.trim().is_empty() {
                    return Err("message is empty".into());
                }
                if self.active_run(*thread_id).is_some() {
                    return Err("a turn is already running in this thread".into());
                }
                let rt = self.rt(*thread_id).map_err(|e| format!("{e:#}"))?;
                let ordinal = rt.next_ordinal;
                rt.next_ordinal += 1;
                batch.events.push(EventKind::RunCreated {
                    run: Run {
                        id: *run_id,
                        thread_id: *thread_id,
                        parent_run_id: None,
                        status: RunStatus::Starting,
                        created_at: now,
                        ended_at: None,
                        error: None,
                    },
                });
                batch.events.push(EventKind::ItemAdded {
                    item: Arc::new(TurnItem {
                        id: *message_id,
                        thread_id: *thread_id,
                        run_id: Some(*run_id),
                        ordinal,
                        created_at: now,
                        kind: ItemKind::UserMessage,
                        text: text.as_str().into(),
                    }),
                });
                if thread.title == DEFAULT_TITLE {
                    batch.events.push(EventKind::ThreadRenamed {
                        thread_id: *thread_id,
                        title: title_from(text),
                    });
                }
                batch.effects.push(Effect::ProviderTurnStart {
                    thread_id: *thread_id,
                    run_id: *run_id,
                    message_id: *message_id,
                });
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
        }
        Ok(batch)
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
        });
        let text = self
            .store
            .item(message_id)?
            .context("message not found")?
            .text;
        if let Err(err) = self.ensure_session(thread_id).await {
            let message = format!("Could not start Codex: {err:#}");
            self.finish_run(thread_id, RunStatus::Failed, Some(message));
            return Ok(());
        }
        let sent = self
            .rt
            .get(&thread_id)
            .and_then(|rt| rt.session.as_ref())
            .map(|live| live.session.prompt(text.to_string()));
        if !matches!(sent, Some(Ok(()))) {
            self.finish_run(
                thread_id,
                RunStatus::Failed,
                Some("The Codex session ended unexpectedly.".into()),
            );
            return Ok(());
        }
        self.set_run_status(thread_id, RunStatus::Running);
        Ok(())
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
        let mut config = SessionConfig::new(&project.path);
        if let Some(exe) = &self.config.codex_executable {
            config = config.executable(exe);
        }
        for (k, v) in &self.config.agent_env {
            config = config.env(k, v);
        }
        let options = codex::CodexOptions {
            resume_thread_id: thread.provider_thread_id.clone(),
            ..codex::CodexOptions::default()
        };
        let mut session = codex::start_with(config, options).await?;
        self.generation += 1;
        let generation = self.generation;
        // Forward this session's events into the core loop, tagged so a
        // released session's stragglers are ignored.
        let mut events = std::mem::replace(&mut session.events, mpsc::channel(1).1);
        let tx = self.session_tx.clone();
        tokio::spawn(async move {
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
        });
        Ok(())
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
            return;
        }
        match msg.event {
            Some(event) => self.on_agent_event(msg.thread_id, event),
            None => {
                let rt = self.rt.get_mut(&msg.thread_id).expect("checked");
                let session = rt.session.take();
                match rt.run.as_ref().map(|r| r.interrupt_deadline.is_some()) {
                    // Exiting is one way to honour an interrupt.
                    Some(true) => self.finish_run(msg.thread_id, RunStatus::Interrupted, None),
                    Some(false) => self.finish_run(
                        msg.thread_id,
                        RunStatus::Failed,
                        Some("The Codex process exited.".into()),
                    ),
                    None => {
                        self.rt.remove(&msg.thread_id);
                    }
                }
                if let Some(live) = session {
                    tokio::spawn(live.session.shutdown());
                }
            }
        }
    }

    fn on_agent_event(&mut self, thread_id: ThreadId, event: AgentEvent) {
        if let AgentEvent::SessionStarted {
            provider_session_id,
        } = event
        {
            let previous = self
                .threads
                .get(&thread_id)
                .and_then(|t| t.provider_thread_id.clone());
            if previous.as_deref() != Some(provider_session_id.as_str()) {
                let mut events = vec![EventKind::ThreadProviderBound {
                    thread_id,
                    provider_thread_id: provider_session_id,
                }];
                if previous.is_some()
                    && let Ok(item) = self.new_item(
                        thread_id,
                        self.rt
                            .get(&thread_id)
                            .and_then(|rt| rt.run.as_ref())
                            .map(|r| r.run_id),
                        ItemKind::SystemNotice {
                            message: "Codex could not resume the previous conversation; \
                                      this turn starts without its context."
                                .into(),
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
            AgentEvent::SessionStarted { .. } => unreachable!(),
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
            Some(rt) if rt.session.is_some() => rt.idle_since = Some(Instant::now()),
            // No process to keep warm: drop the runtime state entirely.
            Some(_) => {
                self.rt.remove(&thread_id);
            }
            None => {}
        }
        eprintln!("blongo: run {} finished ({status:?})", run.run_id);
        self.emit(CoreEvent::RunFinished { thread_id, status });
    }

    // ---------------------------------------------------------------- timers

    fn next_timer(&self) -> Option<Instant> {
        let mut next = self.flush_deadline;
        let mut consider = |t: Instant| {
            next = Some(next.map_or(t, |n: Instant| n.min(t)));
        };
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
        let now = Instant::now();
        if self.flush_deadline.is_some_and(|d| d <= now) {
            self.flush_text(None);
        }
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
            self.release_session(thread_id);
            self.finish_run(
                thread_id,
                RunStatus::Interrupted,
                Some("Codex did not stop in time; its process was terminated.".into()),
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
            self.rt.remove(&thread_id);
        }
    }

    fn release_session(&mut self, thread_id: ThreadId) {
        if let Some(live) = self.rt.get_mut(&thread_id).and_then(|rt| rt.session.take()) {
            tokio::spawn(live.session.shutdown());
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
        let sessions: Vec<Session> = self
            .rt
            .values_mut()
            .filter_map(|rt| rt.session.take())
            .map(|live| live.session)
            .collect();
        let pids: Vec<u32> = sessions.iter().filter_map(Session::pid).collect();
        let all = futures_join(sessions.into_iter().map(Session::shutdown).collect());
        if tokio::time::timeout(Duration::from_secs(5), all)
            .await
            .is_err()
        {
            // Do not leave an agent (or its tool processes) behind.
            for pid in pids {
                blongo_harness::process::kill_group(pid);
            }
        }
    }
}

async fn futures_join(futures: Vec<impl std::future::Future<Output = ()> + Send + 'static>) {
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    for handle in handles {
        let _ = handle.await;
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
