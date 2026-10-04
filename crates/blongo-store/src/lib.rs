//! SQLite event store (rusqlite, WAL).
//!
//! One [`Store::commit`] writes, in a single transaction (t3code's
//! `EventSink.commitCommand` pattern):
//!
//! 1. the domain events, appended to `events` with the next global sequence,
//! 2. the projection rows they imply (`projects`, `threads`, `runs`,
//!    `turn_items`),
//! 3. the command receipt (when the batch answers a client command), and
//! 4. effect outbox rows for the core's effect worker.
//!
//! Replaying a command id that already has a receipt changes nothing and
//! returns [`CommitOutcome::Duplicate`].
//!
//! Message bodies are stored once, in `turn_items.body`. Event payloads
//! carry no bodies (`ItemTextAppended` logs only the resulting length), so
//! the log stays small and streaming never writes a row per token: the core
//! coalesces deltas and commits them every few hundred milliseconds.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use blongo_protocol::{
    CommandId, DomainEvent, EventKind, ItemId, ItemKind, PendingContext, Project, ProjectId,
    ProviderKind, Run, RunId, RunStatus, Schedule, ScheduleId, Thread, ThreadId, ThreadStatus,
    Timestamp, TurnItem, Worktree,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

mod migrations;

pub use migrations::LATEST_VERSION;

/// A side effect the core must perform after a commit. Stored in the outbox
/// in the same transaction as the events that caused it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    /// Start a provider turn for `run_id` with the body of `message_id`.
    ProviderTurnStart {
        thread_id: ThreadId,
        run_id: RunId,
        message_id: ItemId,
    },
    ProviderInterrupt {
        thread_id: ThreadId,
        run_id: RunId,
    },
    RuntimeRequestRespond {
        thread_id: ThreadId,
        run_id: RunId,
        provider_request_id: String,
        approve: bool,
    },
    /// Deliver the body of `message_id` into the running provider turn.
    ProviderSteer {
        thread_id: ThreadId,
        run_id: RunId,
        message_id: ItemId,
    },
    /// Roll the live provider conversation back to before `before_turn`.
    ProviderRewind {
        thread_id: ThreadId,
        before_turn: String,
    },
}

impl Effect {
    fn tag(&self) -> &'static str {
        match self {
            Self::ProviderTurnStart { .. } => "provider_turn_start",
            Self::ProviderInterrupt { .. } => "provider_interrupt",
            Self::RuntimeRequestRespond { .. } => "runtime_request_respond",
            Self::ProviderSteer { .. } => "provider_steer",
            Self::ProviderRewind { .. } => "provider_rewind",
        }
    }

    pub fn thread_id(&self) -> ThreadId {
        match self {
            Self::ProviderTurnStart { thread_id, .. }
            | Self::ProviderInterrupt { thread_id, .. }
            | Self::RuntimeRequestRespond { thread_id, .. }
            | Self::ProviderSteer { thread_id, .. }
            | Self::ProviderRewind { thread_id, .. } => *thread_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectStatus {
    Pending,
    Done,
    /// Process-bound effect left over from a previous process; not retried.
    Dropped,
}

impl EffectStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Dropped => "dropped",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxRow {
    pub id: i64,
    pub effect: Effect,
}

/// What one commit writes.
#[derive(Debug, Default)]
pub struct Batch {
    /// The client command this batch answers (gets a receipt).
    pub command_id: Option<CommandId>,
    pub events: Vec<EventKind>,
    pub effects: Vec<Effect>,
}

impl Batch {
    pub fn for_command(command_id: CommandId) -> Self {
        Self {
            command_id: Some(command_id),
            ..Self::default()
        }
    }

    pub fn event(mut self, event: EventKind) -> Self {
        self.events.push(event);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.effects.is_empty() && self.command_id.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub command_id: CommandId,
    /// Sequence range of the events the command produced (`None` if none).
    pub sequences: Option<(u64, u64)>,
    pub at: Timestamp,
}

#[derive(Debug)]
pub enum CommitOutcome {
    Committed {
        events: Vec<DomainEvent>,
        outbox: Vec<OutboxRow>,
    },
    /// The command id already had a receipt; nothing was written.
    Duplicate(Receipt),
}

pub struct Store {
    conn: Connection,
    last_sequence: u64,
}

impl Store {
    /// Open (creating if needed) and migrate the database at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create data dir {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // A second connection (the off-loop t3code import) may hold the
        // write lock for a moment: wait instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        // Small page cache: the working set is a few open threads.
        conn.pragma_update(None, "cache_size", -1024)?;
        conn.set_prepared_statement_cache_capacity(32);
        migrations::migrate(&conn)?;
        let last_sequence: i64 =
            conn.query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |r| {
                r.get(0)
            })?;
        Ok(Self {
            conn,
            last_sequence: last_sequence as u64,
        })
    }

    pub fn schema_version(&self) -> anyhow::Result<u32> {
        migrations::current_version(&self.conn)
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Commit a batch atomically. On any error nothing is written.
    pub fn commit(&mut self, batch: Batch) -> anyhow::Result<CommitOutcome> {
        if let Some(command_id) = batch.command_id
            && let Some(receipt) = self.receipt(command_id)?
        {
            return Ok(CommitOutcome::Duplicate(receipt));
        }
        let at = Timestamp::now();
        // IMMEDIATE takes the write lock first, so the sequence read below
        // cannot race another connection's commit (the t3code import runs
        // on its own connection, off the core loop).
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: i64 = tx
            .prepare_cached("SELECT COALESCE(MAX(sequence), 0) FROM events")?
            .query_row([], |r| r.get(0))?;
        let mut sequence = self.last_sequence.max(stored as u64);
        let mut events = Vec::with_capacity(batch.events.len());
        for kind in batch.events {
            sequence += 1;
            let event = DomainEvent {
                sequence,
                at,
                command_id: batch.command_id,
                kind,
            };
            append_event(&tx, &event)?;
            apply(&tx, &event)?;
            events.push(event);
        }
        if let Some(command_id) = batch.command_id {
            let range = events
                .first()
                .zip(events.last())
                .map(|(a, b)| (a.sequence as i64, b.sequence as i64));
            tx.prepare_cached(
                "INSERT INTO command_receipts (command_id, first_sequence, last_sequence, at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                command_id.to_string(),
                range.map(|r| r.0),
                range.map(|r| r.1),
                at.0
            ])?;
        }
        let mut outbox = Vec::with_capacity(batch.effects.len());
        for effect in batch.effects {
            tx.prepare_cached(
                "INSERT INTO effect_outbox (thread_id, kind, payload, status, created_at)
                 VALUES (?1, ?2, ?3, 'pending', ?4)",
            )?
            .execute(params![
                effect.thread_id().to_string(),
                effect.tag(),
                serde_json::to_string(&effect)?,
                at.0
            ])?;
            outbox.push(OutboxRow {
                id: tx.last_insert_rowid(),
                effect,
            });
        }
        tx.commit()?;
        self.last_sequence = sequence;
        Ok(CommitOutcome::Committed { events, outbox })
    }

    pub fn receipt(&self, command_id: CommandId) -> anyhow::Result<Option<Receipt>> {
        let row = self
            .conn
            .prepare_cached(
                "SELECT first_sequence, last_sequence, at FROM command_receipts
                 WHERE command_id = ?1",
            )?
            .query_row([command_id.to_string()], |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .optional()?;
        Ok(row.map(|(first, last, at)| Receipt {
            command_id,
            sequences: first.zip(last).map(|(a, b)| (a as u64, b as u64)),
            at: Timestamp(at),
        }))
    }

    /// Committed events after `sequence`, oldest first (bodies excluded).
    pub fn events_after(&self, sequence: u64, limit: usize) -> anyhow::Result<Vec<DomainEvent>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT sequence, at, command_id, payload FROM events
             WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![sequence as i64, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (sequence, at, command_id, payload) = row?;
            out.push(DomainEvent {
                sequence: sequence as u64,
                at: Timestamp(at),
                command_id: command_id.as_deref().and_then(CommandId::parse),
                kind: serde_json::from_str(&payload)
                    .with_context(|| format!("event {sequence} payload"))?,
            });
        }
        Ok(out)
    }

    pub fn projects(&self) -> anyhow::Result<Vec<Project>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, path, created_at FROM projects ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: ProjectId(uuid_col(r, 0)?),
                name: r.get(1)?,
                path: r.get(2)?,
                created_at: Timestamp(r.get(3)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A run by id.
    pub fn run(&self, id: RunId) -> anyhow::Result<Option<Run>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT id, thread_id, parent_run_id, status, created_at, ended_at, error, provider,
                    provider_turn_id, checkpoint, usage
                 FROM runs WHERE id = ?1",
            )?
            .query_row([id.to_string()], run_row)
            .optional()?)
    }

    pub fn project(&self, id: ProjectId) -> anyhow::Result<Option<Project>> {
        Ok(self.projects()?.into_iter().find(|p| p.id == id))
    }

    /// Threads, most recently updated first.
    pub fn threads(&self, include_archived: bool) -> anyhow::Result<Vec<Thread>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, project_id, title, status, archived, created_at, updated_at,
                    provider_thread_id, provider, model, worktree_path, worktree_branch,
                    forked_from, pending_context, parent_thread_id
             FROM threads WHERE archived = 0 OR ?1
             ORDER BY updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([include_archived], thread_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn thread(&self, id: ThreadId) -> anyhow::Result<Option<Thread>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT id, project_id, title, status, archived, created_at, updated_at,
                    provider_thread_id, provider, model, worktree_path, worktree_branch,
                    forked_from, pending_context, parent_thread_id
                 FROM threads WHERE id = ?1",
            )?
            .query_row([id.to_string()], thread_row)
            .optional()?)
    }

    pub fn runs(&self, thread_id: ThreadId) -> anyhow::Result<Vec<Run>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, thread_id, parent_run_id, status, created_at, ended_at, error, provider,
                    provider_turn_id, checkpoint, usage
             FROM runs WHERE thread_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([thread_id.to_string()], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Runs not in a terminal state (across all threads).
    pub fn unfinished_runs(&self) -> anyhow::Result<Vec<Run>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, thread_id, parent_run_id, status, created_at, ended_at, error, provider,
                    provider_turn_id, checkpoint, usage
             FROM runs WHERE status IN ('queued', 'starting', 'running', 'waiting')
             ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], run_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The thread's visible items: those of rolled-back and cancelled runs
    /// are left out.
    pub fn items(&self, thread_id: ThreadId) -> anyhow::Result<Vec<Arc<TurnItem>>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT i.id, i.thread_id, i.run_id, i.ordinal, i.created_at, i.data, i.body
             FROM turn_items i LEFT JOIN runs r ON r.id = i.run_id
             WHERE i.thread_id = ?1
               AND (r.status IS NULL OR r.status NOT IN ('rolled_back', 'cancelled'))
             ORDER BY i.ordinal",
        )?;
        let rows = stmt.query_map([thread_id.to_string()], item_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(Arc::new(row??));
        }
        Ok(out)
    }

    pub fn item(&self, id: ItemId) -> anyhow::Result<Option<TurnItem>> {
        let row = self
            .conn
            .prepare_cached(
                "SELECT id, thread_id, run_id, ordinal, created_at, data, body
                 FROM turn_items WHERE id = ?1",
            )?
            .query_row([id.to_string()], item_row)
            .optional()?;
        row.transpose()
    }

    /// Items of a run that are still streaming or awaiting an answer.
    pub fn open_items(&self, run_id: RunId) -> anyhow::Result<Vec<TurnItem>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, thread_id, run_id, ordinal, created_at, data, body
             FROM turn_items WHERE run_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map([run_id.to_string()], item_row)?;
        let mut out = Vec::new();
        for row in rows {
            let item = row??;
            let open = match &item.kind {
                ItemKind::AssistantMessage { streaming } | ItemKind::Reasoning { streaming } => {
                    *streaming
                }
                ItemKind::ApprovalRequest { state, .. } => {
                    *state == blongo_protocol::ApprovalState::Pending
                }
                ItemKind::CommandExecution { status, .. }
                | ItemKind::FileChange { status, .. }
                | ItemKind::ToolCall { status, .. } => {
                    *status == blongo_protocol::ToolStatus::Running
                }
                _ => false,
            };
            if open {
                out.push(item);
            }
        }
        Ok(out)
    }

    /// Next free ordinal in a thread's timeline.
    pub fn next_ordinal(&self, thread_id: ThreadId) -> anyhow::Result<u32> {
        let max: Option<i64> = self
            .conn
            .prepare_cached("SELECT MAX(ordinal) FROM turn_items WHERE thread_id = ?1")?
            .query_row([thread_id.to_string()], |r| r.get(0))?;
        Ok(max.map_or(0, |m| m as u32 + 1))
    }

    /// Every schedule, oldest first.
    pub fn schedules(&self) -> anyhow::Result<Vec<Schedule>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, project_id, thread_id, cron, prompt, provider, enabled, created_at,
                    next_run_at, last_run_at, last_thread_id, proposed_by
             FROM schedules ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Schedule {
                id: ScheduleId(uuid_col(r, 0)?),
                project_id: ProjectId(uuid_col(r, 1)?),
                thread_id: opt_uuid_col(r, 2)?.map(ThreadId),
                cron: r.get(3)?,
                prompt: r.get(4)?,
                provider: ProviderKind::parse(&r.get::<_, String>(5)?).unwrap_or_default(),
                enabled: r.get(6)?,
                created_at: Timestamp(r.get(7)?),
                next_run_at: r.get::<_, Option<i64>>(8)?.map(Timestamp),
                last_run_at: r.get::<_, Option<i64>>(9)?.map(Timestamp),
                last_thread_id: opt_uuid_col(r, 10)?.map(ThreadId),
                proposed_by: opt_uuid_col(r, 11)?.map(ThreadId),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Re-read the newest sequence (after another connection committed).
    pub fn refresh_last_sequence(&mut self) -> anyhow::Result<u64> {
        let stored: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |r| {
                    r.get(0)
                })?;
        self.last_sequence = self.last_sequence.max(stored as u64);
        Ok(self.last_sequence)
    }

    pub fn pending_effects(&self) -> anyhow::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, payload FROM effect_outbox WHERE status = 'pending' ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, payload) = row?;
            out.push(OutboxRow {
                id,
                effect: serde_json::from_str(&payload)?,
            });
        }
        Ok(out)
    }

    pub fn set_effect_status(&self, id: i64, status: EffectStatus) -> anyhow::Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE effect_outbox SET status = ?2, attempts = attempts + 1 WHERE id = ?1",
            )?
            .execute(params![id, status.as_str()])?;
        Ok(())
    }

    /// Remove an executed effect (a done row has no further use).
    pub fn complete_effect(&self, id: i64) -> anyhow::Result<()> {
        self.conn
            .prepare_cached("DELETE FROM effect_outbox WHERE id = ?1")?
            .execute(params![id])?;
        Ok(())
    }

    /// Delete finished outbox rows (they have no further use).
    pub fn prune_effects(&self) -> anyhow::Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM effect_outbox WHERE status != 'pending'", [])?)
    }
}

fn append_event(tx: &Transaction<'_>, event: &DomainEvent) -> anyhow::Result<()> {
    tx.prepare_cached(
        "INSERT INTO events (sequence, at, thread_id, command_id, kind, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        event.sequence as i64,
        event.at.0,
        event.kind.thread_id().map(|t| t.to_string()),
        event.command_id.map(|c| c.to_string()),
        event.kind.tag(),
        serde_json::to_string(&event.kind)?,
    ])?;
    Ok(())
}

/// Update the projection tables for one event (inside the commit's
/// transaction).
fn apply(tx: &Transaction<'_>, event: &DomainEvent) -> anyhow::Result<()> {
    let at = event.at.0;
    match &event.kind {
        EventKind::ProjectCreated { project } => {
            tx.prepare_cached(
                "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                project.id.to_string(),
                project.name,
                project.path,
                project.created_at.0
            ])?;
        }
        EventKind::ThreadCreated { thread } => {
            tx.prepare_cached(
                "INSERT INTO threads (id, project_id, title, status, archived, created_at,
                                      updated_at, provider_thread_id, provider, model,
                                      worktree_path, worktree_branch, forked_from,
                                      pending_context, parent_thread_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )?
            .execute(params![
                thread.id.to_string(),
                thread.project_id.to_string(),
                thread.title,
                thread_status_str(thread.status),
                thread.archived,
                thread.created_at.0,
                thread.updated_at.0,
                thread.provider_thread_id,
                thread.provider.id(),
                thread.model,
                thread.worktree.as_ref().map(|w| &w.path),
                thread.worktree.as_ref().map(|w| &w.branch),
                thread.forked_from.map(|t| t.to_string()),
                context_json(thread.pending_context.as_ref())?,
                thread.parent_thread_id.map(|t| t.to_string()),
            ])?;
        }
        EventKind::ThreadRenamed { thread_id, title } => {
            expect_one(
                tx.prepare_cached("UPDATE threads SET title = ?2, updated_at = ?3 WHERE id = ?1")?
                    .execute(params![thread_id.to_string(), title, at])?,
                "thread",
            )?;
        }
        EventKind::ThreadArchived { thread_id } => {
            expect_one(
                tx.prepare_cached("UPDATE threads SET archived = 1 WHERE id = ?1")?
                    .execute([thread_id.to_string()])?,
                "thread",
            )?;
        }
        EventKind::ThreadProviderBound {
            thread_id,
            provider_thread_id,
        } => {
            expect_one(
                tx.prepare_cached(
                    "UPDATE threads SET provider_thread_id = ?2, pending_context = NULL
                     WHERE id = ?1",
                )?
                .execute(params![thread_id.to_string(), provider_thread_id])?,
                "thread",
            )?;
        }
        EventKind::ThreadProviderChanged {
            thread_id,
            provider,
            model,
            provider_thread_id,
            pending_context,
        } => {
            expect_one(
                tx.prepare_cached(
                    "UPDATE threads SET provider = ?2, model = ?3, provider_thread_id = ?4,
                                        pending_context = ?5, updated_at = ?6
                     WHERE id = ?1",
                )?
                .execute(params![
                    thread_id.to_string(),
                    provider.id(),
                    model,
                    provider_thread_id,
                    context_json(pending_context.as_ref())?,
                    at
                ])?,
                "thread",
            )?;
        }
        EventKind::RunProviderTurn {
            run_id,
            provider_turn_id,
            ..
        } => {
            expect_one(
                tx.prepare_cached("UPDATE runs SET provider_turn_id = ?2 WHERE id = ?1")?
                    .execute(params![run_id.to_string(), provider_turn_id])?,
                "run",
            )?;
        }
        EventKind::RunCheckpointed { run_id, commit, .. } => {
            expect_one(
                tx.prepare_cached("UPDATE runs SET checkpoint = ?2 WHERE id = ?1")?
                    .execute(params![run_id.to_string(), commit])?,
                "run",
            )?;
        }
        EventKind::RunCreated { run } => {
            tx.prepare_cached(
                "INSERT INTO runs (id, thread_id, parent_run_id, status, created_at, ended_at,
                                   error, provider, provider_turn_id, checkpoint, usage)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                run.id.to_string(),
                run.thread_id.to_string(),
                run.parent_run_id.map(|r| r.to_string()),
                run_status_str(run.status),
                run.created_at.0,
                run.ended_at.map(|t| t.0),
                run.error,
                run.provider.id(),
                run.provider_turn_id,
                run.checkpoint,
                run.usage.as_ref().map(serde_json::to_string).transpose()?,
            ])?;
            if run.parent_run_id.is_none()
                && let Some(status) = run.status.thread_status()
            {
                set_thread_status(tx, run.thread_id, status, at)?;
            }
        }
        EventKind::RunStatusChanged {
            thread_id,
            run_id,
            status,
            error,
        } => {
            let ended = status.is_terminal().then_some(at);
            expect_one(
                tx.prepare_cached(
                    "UPDATE runs SET status = ?2, error = COALESCE(?3, error),
                                     ended_at = COALESCE(?4, ended_at)
                     WHERE id = ?1",
                )?
                .execute(params![
                    run_id.to_string(),
                    run_status_str(*status),
                    error,
                    ended
                ])?,
                "run",
            )?;
            let parent: Option<String> = tx
                .prepare_cached("SELECT parent_run_id FROM runs WHERE id = ?1")?
                .query_row([run_id.to_string()], |r| r.get(0))?;
            // Invariant: only a root run moves the thread (and ends a turn).
            if parent.is_none()
                && let Some(status) = status.thread_status()
            {
                set_thread_status(tx, *thread_id, status, at)?;
            }
        }
        EventKind::ItemAdded { item } => {
            tx.prepare_cached(
                "INSERT INTO turn_items (id, thread_id, run_id, ordinal, kind, created_at,
                                         updated_at, data, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
            )?
            .execute(params![
                item.id.to_string(),
                item.thread_id.to_string(),
                item.run_id.map(|r| r.to_string()),
                item.ordinal,
                item.kind.tag(),
                item.created_at.0,
                serde_json::to_string(&item.kind)?,
                &*item.text,
            ])?;
        }
        EventKind::ItemUpdated { item } => {
            expect_one(
                tx.prepare_cached(
                    "UPDATE turn_items SET kind = ?2, data = ?3, updated_at = ?4, ordinal = ?5,
                     run_id = ?6 WHERE id = ?1",
                )?
                .execute(params![
                    item.id.to_string(),
                    item.kind.tag(),
                    serde_json::to_string(&item.kind)?,
                    at,
                    item.ordinal,
                    item.run_id.map(|r| r.to_string()),
                ])?,
                "item",
            )?;
        }
        EventKind::ItemTextAppended { item_id, chunk, .. } => {
            expect_one(
                tx.prepare_cached(
                    "UPDATE turn_items SET body = body || ?2, updated_at = ?3 WHERE id = ?1",
                )?
                .execute(params![item_id.to_string(), &**chunk, at])?,
                "item",
            )?;
        }
        EventKind::RunUsage { run_id, usage, .. } => {
            expect_one(
                tx.prepare_cached("UPDATE runs SET usage = ?2 WHERE id = ?1")?
                    .execute(params![run_id.to_string(), serde_json::to_string(usage)?])?,
                "run",
            )?;
        }
        EventKind::ScheduleCreated { schedule } | EventKind::ScheduleUpdated { schedule } => {
            tx.prepare_cached(
                "INSERT INTO schedules (id, project_id, thread_id, cron, prompt, provider, enabled,
                                        created_at, next_run_at, last_run_at, last_thread_id,
                                        proposed_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (id) DO UPDATE SET
                     thread_id = excluded.thread_id, cron = excluded.cron,
                     prompt = excluded.prompt, provider = excluded.provider,
                     enabled = excluded.enabled, next_run_at = excluded.next_run_at,
                     last_run_at = excluded.last_run_at, last_thread_id = excluded.last_thread_id,
                     proposed_by = excluded.proposed_by",
            )?
            .execute(params![
                schedule.id.to_string(),
                schedule.project_id.to_string(),
                schedule.thread_id.map(|t| t.to_string()),
                schedule.cron,
                schedule.prompt,
                schedule.provider.id(),
                schedule.enabled,
                schedule.created_at.0,
                schedule.next_run_at.map(|t| t.0),
                schedule.last_run_at.map(|t| t.0),
                schedule.last_thread_id.map(|t| t.to_string()),
                schedule.proposed_by.map(|t| t.to_string()),
            ])?;
        }
        EventKind::ScheduleDeleted { schedule_id } => {
            expect_one(
                tx.prepare_cached("DELETE FROM schedules WHERE id = ?1")?
                    .execute([schedule_id.to_string()])?,
                "schedule",
            )?;
        }
        EventKind::ItemFinished { item_id, .. } => {
            let data: String = tx
                .prepare_cached("SELECT data FROM turn_items WHERE id = ?1")?
                .query_row([item_id.to_string()], |r| r.get(0))
                .optional()?
                .with_context(|| format!("unknown item {item_id}"))?;
            let mut kind: ItemKind = serde_json::from_str(&data)?;
            match &mut kind {
                ItemKind::AssistantMessage { streaming } | ItemKind::Reasoning { streaming } => {
                    *streaming = false
                }
                _ => {}
            }
            tx.prepare_cached("UPDATE turn_items SET data = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![
                    item_id.to_string(),
                    serde_json::to_string(&kind)?,
                    at
                ])?;
        }
    }
    Ok(())
}

fn expect_one(changed: usize, what: &str) -> anyhow::Result<()> {
    if changed != 1 {
        bail!("{what} not found");
    }
    Ok(())
}

fn set_thread_status(
    tx: &Transaction<'_>,
    thread_id: ThreadId,
    status: ThreadStatus,
    at: i64,
) -> anyhow::Result<()> {
    expect_one(
        tx.prepare_cached("UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1")?
            .execute(params![
                thread_id.to_string(),
                thread_status_str(status),
                at
            ])?,
        "thread",
    )
}

fn thread_status_str(status: ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Idle => "idle",
        ThreadStatus::Running => "running",
        ThreadStatus::Waiting => "waiting",
        ThreadStatus::Failed => "failed",
    }
}

fn parse_thread_status(s: &str) -> ThreadStatus {
    match s {
        "running" => ThreadStatus::Running,
        "waiting" => ThreadStatus::Waiting,
        "failed" => ThreadStatus::Failed,
        _ => ThreadStatus::Idle,
    }
}

fn context_json(context: Option<&PendingContext>) -> anyhow::Result<Option<String>> {
    Ok(context.map(serde_json::to_string).transpose()?)
}

fn run_status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Starting => "starting",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Completed => "completed",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::RolledBack => "rolled_back",
    }
}

fn parse_run_status(s: &str) -> RunStatus {
    match s {
        "starting" => RunStatus::Starting,
        "running" => RunStatus::Running,
        "waiting" => RunStatus::Waiting,
        "completed" => RunStatus::Completed,
        "interrupted" => RunStatus::Interrupted,
        "queued" => RunStatus::Queued,
        "cancelled" => RunStatus::Cancelled,
        "rolled_back" => RunStatus::RolledBack,
        _ => RunStatus::Failed,
    }
}

fn uuid_col(r: &Row<'_>, ix: usize) -> rusqlite::Result<blongo_protocol::Uuid> {
    let s: String = r.get(ix)?;
    s.parse().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(ix, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn opt_uuid_col(r: &Row<'_>, ix: usize) -> rusqlite::Result<Option<blongo_protocol::Uuid>> {
    match r.get::<_, Option<String>>(ix)? {
        None => Ok(None),
        Some(_) => uuid_col(r, ix).map(Some),
    }
}

fn thread_row(r: &Row<'_>) -> rusqlite::Result<Thread> {
    let worktree = match (
        r.get::<_, Option<String>>(10)?,
        r.get::<_, Option<String>>(11)?,
    ) {
        (Some(path), Some(branch)) => Some(Worktree { path, branch }),
        _ => None,
    };
    Ok(Thread {
        id: ThreadId(uuid_col(r, 0)?),
        project_id: ProjectId(uuid_col(r, 1)?),
        title: r.get(2)?,
        status: parse_thread_status(&r.get::<_, String>(3)?),
        archived: r.get(4)?,
        created_at: Timestamp(r.get(5)?),
        updated_at: Timestamp(r.get(6)?),
        provider_thread_id: r.get(7)?,
        provider: ProviderKind::parse(&r.get::<_, String>(8)?).unwrap_or_default(),
        model: r.get(9)?,
        worktree,
        forked_from: opt_uuid_col(r, 12)?.map(ThreadId),
        // An unreadable context degrades to a text handoff, never an error.
        pending_context: r
            .get::<_, Option<String>>(13)?
            .map(|json| serde_json::from_str(&json).unwrap_or(PendingContext::Handoff)),
        parent_thread_id: opt_uuid_col(r, 14)?.map(ThreadId),
    })
}

fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: RunId(uuid_col(r, 0)?),
        thread_id: ThreadId(uuid_col(r, 1)?),
        parent_run_id: opt_uuid_col(r, 2)?.map(RunId),
        status: parse_run_status(&r.get::<_, String>(3)?),
        created_at: Timestamp(r.get(4)?),
        ended_at: r.get::<_, Option<i64>>(5)?.map(Timestamp),
        error: r.get(6)?,
        provider: ProviderKind::parse(&r.get::<_, String>(7)?).unwrap_or_default(),
        provider_turn_id: r.get(8)?,
        checkpoint: r.get(9)?,
        usage: r
            .get::<_, Option<String>>(10)?
            .and_then(|json| serde_json::from_str(&json).ok()),
    })
}

/// Outer error: SQL; inner: JSON in `data`.
fn item_row(r: &Row<'_>) -> rusqlite::Result<anyhow::Result<TurnItem>> {
    let data: String = r.get(5)?;
    let body: String = r.get(6)?;
    let id = ItemId(uuid_col(r, 0)?);
    let thread_id = ThreadId(uuid_col(r, 1)?);
    let run_id = opt_uuid_col(r, 2)?.map(RunId);
    let ordinal: u32 = r.get(3)?;
    let created_at = Timestamp(r.get(4)?);
    Ok(serde_json::from_str::<ItemKind>(&data)
        .with_context(|| format!("item {id} data"))
        .map(|kind| TurnItem {
            id,
            thread_id,
            run_id,
            ordinal,
            created_at,
            kind,
            text: body.into(),
        }))
}

#[cfg(test)]
mod tests;
