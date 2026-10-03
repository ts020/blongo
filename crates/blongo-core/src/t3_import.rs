//! One-way, read-only import of t3code's local history (`statev2.sqlite`).
//!
//! t3code's database is never opened in place: the file (and its `-wal`
//! when present) is copied to a temporary directory and the copy is opened
//! read-only, so neither the database nor its WAL/SHM files can be touched
//! even by SQLite's own recovery. Tables read (t3code migration
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
//! what is new. Imported threads have no provider session: their next
//! message hands the conversation over as context.

use std::collections::HashMap;
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

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub projects: usize,
    pub threads: usize,
    pub runs: usize,
    pub items: usize,
    /// Threads already imported earlier.
    pub skipped_threads: usize,
}

/// t3code's default database location (`~/.t3/userdata/statev2.sqlite`).
pub fn default_source() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".t3/userdata/statev2.sqlite"))
}

fn derived(kind: &str, id: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, format!("t3code:{kind}:{id}").as_bytes())
}

/// Copy `source` (+ `-wal`) into a private temp dir and open the copy.
fn open_copy(source: &Path) -> anyhow::Result<(Connection, PathBuf)> {
    if !source.is_file() {
        bail!("{} does not exist", source.display());
    }
    let dir = std::env::temp_dir().join(format!(
        "blongo-t3-import-{}-{}",
        std::process::id(),
        Timestamp::now().0
    ));
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join("statev2.sqlite");
    std::fs::copy(source, &copy).with_context(|| format!("copying {}", source.display()))?;
    let wal = PathBuf::from(format!("{}-wal", source.display()));
    if wal.is_file() {
        std::fs::copy(&wal, dir.join("statev2.sqlite-wal"))?;
    }
    // The copy may need its WAL replayed, so it is opened writable; the
    // original is never opened at all.
    let conn = Connection::open_with_flags(
        &copy,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok((conn, dir))
}

/// Import everything new from `source` into `store`.
pub fn import(store: &mut Store, source: &Path) -> anyhow::Result<ImportReport> {
    let (conn, tmp) = open_copy(source)?;
    let result = import_from(store, &conn);
    drop(conn);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

struct T3Project {
    id: String,
    title: String,
    root: String,
}

fn import_from(store: &mut Store, conn: &Connection) -> anyhow::Result<ImportReport> {
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
        stmt.query_map([], |r| {
            Ok(T3Project {
                id: r.get(0)?,
                title: r.get(1)?,
                root: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?
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
    let threads: Vec<Row> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    for (t3_id, t3_project, title, provider, created, updated, archived) in threads {
        let Some(project_id) = project_ids.get(&t3_project).copied() else {
            continue;
        };
        let thread_id = ThreadId(derived("thread", &t3_id));
        if store.thread(thread_id)?.is_some() {
            report.skipped_threads += 1;
            continue;
        }
        let mut events = Vec::new();
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
        // No provider session to resume: the next message carries the
        // conversation.
        thread.pending_context = Some(PendingContext::Handoff);
        events.push(EventKind::ThreadCreated { thread });

        let mut runs_stmt = conn.prepare_cached(
            "SELECT run_id, provider, status, requested_at, completed_at
             FROM orchestration_v2_projection_runs WHERE thread_id = ?1 ORDER BY ordinal",
        )?;
        let runs: Vec<(String, String, String, String, Option<String>)> = runs_stmt
            .query_map([&t3_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut run_ids = HashMap::new();
        let mut last_created = i64::MIN;
        for (t3_run, run_provider, status, requested, completed) in runs {
            let id = RunId(derived("run", &t3_run));
            run_ids.insert(t3_run, id);
            // Runs are listed by creation time: keep t3code's order even
            // when timestamps tie.
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
            report.runs += 1;
        }

        let mut items_stmt = conn.prepare_cached(
            "SELECT turn_item_id, run_id, type, updated_at, payload_json
             FROM orchestration_v2_projection_turn_items
             WHERE thread_id = ?1 ORDER BY ordinal, turn_item_id",
        )?;
        let rows: Vec<(String, Option<String>, String, String, String)> = items_stmt
            .query_map([&t3_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut ordinal = 0u32;
        for (t3_item, t3_run, kind, updated_at, payload) in rows {
            let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            let Some((kind, text)) = item_kind(&kind, &payload) else {
                continue;
            };
            let run_id = t3_run.and_then(|r| run_ids.get(&r).copied());
            events.push(EventKind::ItemAdded {
                item: Arc::new(TurnItem {
                    id: ItemId(derived("item", &t3_item)),
                    thread_id,
                    run_id,
                    ordinal,
                    created_at: parse_time(&updated_at),
                    kind,
                    text: text.into(),
                }),
            });
            ordinal += 1;
            report.items += 1;
        }
        store.commit(Batch {
            command_id: None,
            events,
            effects: vec![],
        })?;
        report.threads += 1;
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
