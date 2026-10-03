//! One-way, read-only import of t3code's local history (`statev2.sqlite`).
//!
//! Everything is read from a snapshot in a private (0700) temporary
//! directory. While t3code is running (its `-wal` exists), the snapshot is
//! taken with SQLite's online backup API through a read-only connection,
//! which sees one committed state even while t3code keeps writing. When
//! there is no `-wal` (t3code closed cleanly), opening the file at all
//! would make SQLite create `-wal`/`-shm` next to it, so the file is copied
//! instead and the copy integrity-checked. Tables read (t3code migration
//! 005_Projections and 055_OrchestrationV2):
//!
//! - `projection_projects` (project_id, title, workspace_root, deleted_at)
//! - `orchestration_v2_projection_threads` (thread_id, project_id, title,
//!   default_provider, created_at, updated_at, archived_at, deleted_at)
//! - `orchestration_v2_projection_runs` (run_id, thread_id, ordinal,
//!   provider, status, requested_at, completed_at)
//! - `orchestration_v2_projection_turn_items` (thread_id, run_id, ordinal,
//!   type, payload_json): user / assistant messages, reasoning, commands,
//!   file changes, todo lists, notices and errors; other kinds are skipped.
//!
//! Ids are derived from t3code's (UUID v5), so importing again only adds
//! what is new: new threads, and new turns and items of threads imported
//! before (appended after what the thread already has; a thread that is
//! running in Blongo is left alone until the next import). Rows Blongo
//! cannot read (NULLs in required columns) are skipped and counted.
//! Imported threads have no provider session: their next message hands the
//! conversation over as context.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use blongo_protocol::{
    EventKind, ItemId, ItemKind, PendingContext, PlanStatus, PlanStep, Project, ProjectId,
    ProviderKind, Run, RunId, RunStatus, Thread, ThreadId, Timestamp, ToolStatus, TurnItem, Uuid,
};
use blongo_store::{Batch, Store};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

/// Namespace of the derived ids.
const NAMESPACE: Uuid = Uuid::from_u128(0x6f3c_7a52_1d0e_4b8e_9a51_2c4d_8e7f_b10c);

pub use blongo_protocol::client::ImportReport;

/// t3code's default database location (`~/.t3/userdata/statev2.sqlite`).
pub fn default_source() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".t3/userdata/statev2.sqlite"))
}

fn derived(kind: &str, id: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, format!("t3code:{kind}:{id}").as_bytes())
}

/// A private (0700 on Unix) temporary directory, removed on drop.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::DirBuilderExt;
        let base = std::env::temp_dir();
        for attempt in 0..16u32 {
            let dir = base.join(format!(
                "blongo-t3-import-{}-{}-{attempt}",
                std::process::id(),
                Timestamp::now().0
            ));
            // `create` (not `create_all`): fails if someone else made it.
            #[cfg(unix)]
            let created = std::fs::DirBuilder::new().mode(0o700).create(&dir);
            // Elsewhere the per-user temp folder's ACL keeps it private.
            #[cfg(not(unix))]
            let created = std::fs::DirBuilder::new().create(&dir);
            match created {
                Ok(()) => return Ok(Self(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("creating a temporary directory"),
            }
        }
        bail!("could not create a private temporary directory")
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Snapshot `source` with the online backup API into `dir` and open the
/// snapshot.
fn open_snapshot(source: &Path, dir: &PrivateDir) -> anyhow::Result<Connection> {
    if !source.is_file() {
        bail!("{} does not exist", source.display());
    }
    let copy = dir.0.join("statev2.sqlite");
    let wal = PathBuf::from(format!("{}-wal", source.display()));
    if !wal.exists() {
        std::fs::copy(source, &copy).with_context(|| format!("copying {}", source.display()))?;
        let conn = Connection::open(&copy)?;
        let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            bail!(
                "{} changed while it was read (is t3code starting?); try again",
                source.display()
            );
        }
        return Ok(conn);
    }
    let original = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", source.display()))?;
    original.busy_timeout(std::time::Duration::from_secs(5))?;
    let mut conn = Connection::open(&copy)?;
    {
        let backup = rusqlite::backup::Backup::new(&original, &mut conn)?;
        backup
            .run_to_completion(256, std::time::Duration::from_millis(5), None)
            .with_context(|| format!("reading {}", source.display()))?;
    }
    drop(original);
    Ok(conn)
}

/// Import everything new from `source` into `store`. `busy` threads (a run
/// or queue in Blongo) do not get new turns this time.
pub fn import(
    store: &mut Store,
    source: &Path,
    busy: &dyn Fn(ThreadId) -> bool,
) -> anyhow::Result<ImportReport> {
    let dir = PrivateDir::new()?;
    let conn = open_snapshot(source, &dir)?;
    let result = import_from(store, &conn, busy);
    drop(conn);
    drop(dir);
    result
}

/// Collect readable rows; count the rest.
fn rows<T>(iter: impl Iterator<Item = rusqlite::Result<Option<T>>>, bad: &mut usize) -> Vec<T> {
    let mut out = Vec::new();
    for row in iter {
        match row {
            Ok(Some(row)) => out.push(row),
            Ok(None) | Err(_) => *bad += 1,
        }
    }
    out
}

struct T3Project {
    id: String,
    title: String,
    root: String,
}

fn import_from(
    store: &mut Store,
    conn: &Connection,
    busy: &dyn Fn(ThreadId) -> bool,
) -> anyhow::Result<ImportReport> {
    let mut report = ImportReport::default();
    let has = |table: &str| -> anyhow::Result<bool> {
        Ok(conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };
    if !has("orchestration_v2_projection_threads")? {
        bail!("not a t3code statev2 database (no orchestration_v2 tables)");
    }
    let projects: Vec<T3Project> = if has("projection_projects")? {
        let mut stmt = conn.prepare(
            "SELECT project_id, title, workspace_root FROM projection_projects
             WHERE deleted_at IS NULL ORDER BY created_at",
        )?;
        let iter = stmt.query_map([], |r| {
            let (Some(id), Some(root)) = (r.get::<_, Option<String>>(0)?, r.get(2)?) else {
                return Ok(None);
            };
            Ok(Some(T3Project {
                id,
                title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                root,
            }))
        })?;
        rows(iter, &mut report.bad_rows)
    } else {
        Vec::new()
    };
    let mut existing: HashMap<String, ProjectId> = store
        .projects()?
        .into_iter()
        .map(|p| (p.path.clone(), p.id))
        .collect();
    let mut project_ids = HashMap::new();
    for project in &projects {
        let path = std::fs::canonicalize(&project.root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| project.root.clone());
        let id = match existing.get(&path) {
            Some(id) => *id,
            None => {
                let id = ProjectId(derived("project", &project.id));
                store.commit(Batch {
                    command_id: None,
                    events: vec![EventKind::ProjectCreated {
                        project: Project {
                            id,
                            name: if project.title.trim().is_empty() {
                                path.rsplit('/').next().unwrap_or(&path).to_owned()
                            } else {
                                project.title.clone()
                            },
                            path: path.clone(),
                            created_at: Timestamp::now(),
                        },
                    }],
                    effects: vec![],
                })?;
                existing.insert(path, id);
                report.projects += 1;
                id
            }
        };
        project_ids.insert(project.id.clone(), id);
    }

    let mut stmt = conn.prepare(
        "SELECT thread_id, project_id, title, default_provider, created_at, updated_at,
                archived_at
         FROM orchestration_v2_projection_threads
         WHERE deleted_at IS NULL ORDER BY created_at",
    )?;
    // (id, project, title, provider, created, updated, archived)
    type Row = (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
    );
    let iter = stmt.query_map([], |r| {
        let (Some(id), Some(project)) = (
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ) else {
            return Ok(None);
        };
        let text = |i: usize| -> rusqlite::Result<String> {
            Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default())
        };
        Ok(Some((
            id,
            project,
            text(2)?,
            text(3)?,
            text(4)?,
            text(5)?,
            r.get(6)?,
        )))
    })?;
    let threads: Vec<Row> = rows(iter, &mut report.bad_rows);
    drop(stmt);
    for (t3_id, t3_project, title, provider, created, updated, archived) in threads {
        let Some(project_id) = project_ids.get(&t3_project).copied() else {
            continue;
        };
        let thread_id = ThreadId(derived("thread", &t3_id));
        let mut events = Vec::new();
        let existing = store.thread(thread_id)?;
        // What an earlier import (or Blongo itself) already has.
        let (known_runs, known_items, mut ordinal, mut last_created) = match &existing {
            Some(_) if busy(thread_id) => {
                report.skipped_threads += 1;
                continue;
            }
            Some(_) => {
                let runs = store.runs(thread_id)?;
                let items = store.items(thread_id)?;
                let last = runs
                    .iter()
                    .map(|r| r.created_at.0)
                    .max()
                    .unwrap_or(i64::MIN);
                let next = items.iter().map(|i| i.ordinal + 1).max().unwrap_or(0);
                (
                    runs.into_iter().map(|r| r.id).collect::<HashSet<_>>(),
                    items.into_iter().map(|i| i.id).collect::<HashSet<_>>(),
                    next,
                    last,
                )
            }
            None => {
                let mut thread = Thread::new(
                    thread_id,
                    project_id,
                    if title.trim().is_empty() {
                        "Imported thread"
                    } else {
                        title.trim()
                    },
                    parse_time(&created),
                );
                thread.updated_at = parse_time(&updated);
                thread.provider = provider_kind(&provider);
                thread.archived = archived.is_some();
                // No provider session to resume: the next message carries
                // the conversation.
                thread.pending_context = Some(PendingContext::Handoff);
                events.push(EventKind::ThreadCreated { thread });
                (HashSet::new(), HashSet::new(), 0u32, i64::MIN)
            }
        };

        let mut runs_stmt = conn.prepare_cached(
            "SELECT run_id, provider, status, requested_at, completed_at
             FROM orchestration_v2_projection_runs WHERE thread_id = ?1 ORDER BY ordinal",
        )?;
        let iter = runs_stmt.query_map([&t3_id], |r| {
            let Some(id) = r.get::<_, Option<String>>(0)? else {
                return Ok(None);
            };
            let text = |i: usize| -> rusqlite::Result<String> {
                Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default())
            };
            Ok(Some((
                id,
                text(1)?,
                text(2)?,
                text(3)?,
                r.get::<_, Option<String>>(4)?,
            )))
        })?;
        let runs = rows(iter, &mut report.bad_rows);
        let mut run_ids = HashMap::new();
        let mut new_runs = 0;
        for (t3_run, run_provider, status, requested, completed) in runs {
            let id = RunId(derived("run", &t3_run));
            run_ids.insert(t3_run, id);
            if known_runs.contains(&id) {
                continue;
            }
            // Runs are listed by creation time: keep t3code's order even
            // when timestamps tie, after the runs the thread already has.
            let created = parse_time(&requested).0.max(last_created.saturating_add(1));
            last_created = created;
            let mut run = Run::new(
                id,
                thread_id,
                run_status(&status),
                provider_kind(&run_provider),
                Timestamp(created),
            );
            run.ended_at = completed.as_deref().map(parse_time);
            events.push(EventKind::RunCreated { run });
            new_runs += 1;
        }

        let mut items_stmt = conn.prepare_cached(
            "SELECT turn_item_id, run_id, type, updated_at, payload_json
             FROM orchestration_v2_projection_turn_items
             WHERE thread_id = ?1 ORDER BY ordinal, turn_item_id",
        )?;
        let iter = items_stmt.query_map([&t3_id], |r| {
            let (Some(id), Some(kind)) = (
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(2)?,
            ) else {
                return Ok(None);
            };
            Ok(Some((
                id,
                r.get::<_, Option<String>>(1)?,
                kind,
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            )))
        })?;
        let item_rows = rows(iter, &mut report.bad_rows);
        let mut new_items = 0;
        for (t3_item, t3_run, kind, updated_at, payload) in item_rows {
            let id = ItemId(derived("item", &t3_item));
            if known_items.contains(&id) {
                continue;
            }
            let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            let Some((kind, text)) = item_kind(&kind, &payload) else {
                continue;
            };
            let run_id = t3_run.and_then(|r| run_ids.get(&r).copied());
            events.push(EventKind::ItemAdded {
                item: Arc::new(TurnItem {
                    id,
                    thread_id,
                    run_id,
                    ordinal,
                    created_at: parse_time(&updated_at),
                    kind,
                    text: text.into(),
                }),
            });
            ordinal += 1;
            new_items += 1;
        }
        if let Some(thread) = &existing {
            if new_runs == 0 && new_items == 0 {
                report.skipped_threads += 1;
                continue;
            }
            // The provider never saw these turns: the next message hands
            // the whole conversation over again.
            if thread.provider_thread_id.is_some()
                || thread.pending_context != Some(PendingContext::Handoff)
            {
                events.push(EventKind::ThreadProviderChanged {
                    thread_id,
                    provider: thread.provider,
                    model: thread.model.clone(),
                    provider_thread_id: None,
                    pending_context: Some(PendingContext::Handoff),
                });
            }
            report.updated_threads.push(thread_id);
        } else {
            report.threads += 1;
        }
        report.runs += new_runs;
        report.items += new_items;
        store.commit(Batch {
            command_id: None,
            events,
            effects: vec![],
        })?;
    }
    Ok(report)
}

fn provider_kind(t3: &str) -> ProviderKind {
    let t3 = t3.to_ascii_lowercase();
    if t3.contains("claude") {
        ProviderKind::ClaudeCode
    } else if t3.contains("antigravity") || t3.contains("agy") {
        ProviderKind::Antigravity
    } else {
        ProviderKind::Codex
    }
}

fn run_status(t3: &str) -> RunStatus {
    match t3 {
        "completed" => RunStatus::Completed,
        "failed" => RunStatus::Failed,
        "cancelled" => RunStatus::Cancelled,
        "rolled_back" => RunStatus::RolledBack,
        // Anything unfinished in t3code is not running here.
        _ => RunStatus::Interrupted,
    }
}

fn str_field(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Blongo's item for a t3code turn item (`None`: not imported).
fn item_kind(kind: &str, payload: &Value) -> Option<(ItemKind, String)> {
    Some(match kind {
        "user_message" => (ItemKind::UserMessage, str_field(payload, "text")),
        "assistant_message" => (
            ItemKind::AssistantMessage { streaming: false },
            str_field(payload, "text"),
        ),
        "reasoning" => (
            ItemKind::Reasoning { streaming: false },
            str_field(payload, "text"),
        ),
        "command_execution" => {
            let failed = payload
                .get("outputIndicatesFailure")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (
                ItemKind::CommandExecution {
                    call_id: str_field(payload, "id"),
                    command: str_field(payload, "input"),
                    status: if failed {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Completed
                    },
                    output: crate_preview(&str_field(payload, "output")),
                    exit_code: payload
                        .get("exitCode")
                        .and_then(Value::as_i64)
                        .map(|c| c as i32),
                },
                String::new(),
            )
        }
        "file_change" => (
            ItemKind::FileChange {
                call_id: str_field(payload, "id"),
                paths: vec![str_field(payload, "fileName")],
                status: ToolStatus::Completed,
            },
            String::new(),
        ),
        "todo_list" => (
            ItemKind::Plan {
                steps: payload
                    .get("steps")
                    .and_then(Value::as_array)
                    .map(|steps| {
                        steps
                            .iter()
                            .map(|s| PlanStep {
                                text: str_field(s, "text"),
                                status: PlanStatus::parse(&str_field(s, "status")),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            String::new(),
        ),
        "system_notice" => (
            ItemKind::SystemNotice {
                message: str_field(payload, "message"),
            },
            String::new(),
        ),
        "error" => (
            ItemKind::Error {
                message: payload
                    .pointer("/failure/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Error")
                    .to_owned(),
            },
            String::new(),
        ),
        _ => return None,
    })
}

fn crate_preview(text: &str) -> String {
    const MAX: usize = 4 * 1024;
    if text.len() <= MAX {
        return text.to_owned();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// `2026-05-01T12:34:56.789Z` (or without fraction / with an offset of
/// `+00:00`) to milliseconds; anything unparsable is "now".
fn parse_time(s: &str) -> Timestamp {
    fn num(s: &str) -> Option<i64> {
        s.parse().ok()
    }
    let parsed = (|| {
        let (date, time) = s.split_once('T')?;
        let mut d = date.split('-');
        let (y, m, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
        let time = time.trim_end_matches('Z').trim_end_matches("+00:00");
        let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
        let mut t = hms.split(':');
        let (h, mi, sec) = (num(t.next()?)?, num(t.next()?)?, num(t.next()?)?);
        let ms = num(&format!("{:0<3}", &frac[..frac.len().min(3)]))?;
        // Days from civil (Howard Hinnant's algorithm).
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some(((days * 86_400 + h * 3_600 + mi * 60 + sec) * 1_000) + ms)
    })();
    parsed.map(Timestamp).unwrap_or_else(Timestamp::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("1970-01-01T00:00:00.000Z"), Timestamp(0));
        assert_eq!(
            parse_time("2026-05-01T12:34:56.789Z"),
            Timestamp(1_777_638_896_789)
        );
        assert_eq!(
            parse_time("2026-05-01T12:34:56Z"),
            Timestamp(1_777_638_896_000)
        );
    }

    #[test]
    fn providers_map() {
        assert_eq!(provider_kind("claudeAgent"), ProviderKind::ClaudeCode);
        assert_eq!(provider_kind("codex"), ProviderKind::Codex);
        assert_eq!(provider_kind("antigravity"), ProviderKind::Antigravity);
    }
}
