//! Import of a t3code `statev2.sqlite`, against a synthetic database built
//! with t3code's own table definitions (migrations 005_Projections and
//! 055_OrchestrationV2, the columns the import reads plus their NOT NULL
//! neighbours). Never touches a real ~/.t3.

use std::path::{Path, PathBuf};

use blongo_core::Store;
use blongo_core::t3_import::{ImportReport, import};
use blongo_protocol::{ItemKind, PendingContext, PlanStatus, ProviderKind, RunStatus};
use rusqlite::{Connection, params};

const SCHEMA: &str = "
CREATE TABLE projection_projects (
  project_id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace_root TEXT NOT NULL,
  default_model TEXT, scripts_json TEXT NOT NULL, created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL, deleted_at TEXT);
CREATE TABLE orchestration_v2_projection_threads (
  thread_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
  default_provider TEXT NOT NULL, runtime_mode TEXT NOT NULL, interaction_mode TEXT NOT NULL,
  active_provider_thread_id TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  archived_at TEXT, deleted_at TEXT, payload_json TEXT NOT NULL);
CREATE TABLE orchestration_v2_projection_runs (
  run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
  provider TEXT NOT NULL, provider_thread_id TEXT, status TEXT NOT NULL,
  requested_at TEXT NOT NULL, completed_at TEXT, payload_json TEXT NOT NULL);
CREATE TABLE orchestration_v2_projection_turn_items (
  turn_item_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, run_id TEXT, node_id TEXT,
  provider_thread_id TEXT, provider_turn_id TEXT, parent_item_id TEXT,
  ordinal INTEGER NOT NULL, type TEXT NOT NULL, status TEXT NOT NULL,
  updated_at TEXT NOT NULL, payload_json TEXT NOT NULL);
";

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "blongo-t3-import-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn synthetic_db(dir: &Path, workspace: &Path) -> PathBuf {
    let path = dir.join("statev2.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    let now = "2026-05-01T12:00:00.000Z";
    conn.execute(
        "INSERT INTO projection_projects VALUES ('p1', 'Demo', ?1, NULL, '[]', ?2, ?2, NULL)",
        params![workspace.to_string_lossy(), now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projection_projects VALUES ('p2', 'Gone', '/nowhere', NULL, '[]', ?1, ?1, ?1)",
        [now],
    )
    .unwrap();
    for (id, title, provider, archived, deleted) in [
        ("t1", "Fix the build", "codex", None, None),
        ("t2", "Claude chat", "claudeAgent", Some(now), None),
        ("t3", "Deleted", "codex", None, Some(now)),
    ] {
        conn.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES
             (?1, 'p1', ?2, ?3, 'full-access', 'default', 'prov-1', ?4, ?4, ?5, ?6, '{}')",
            params![id, title, provider, now, archived, deleted],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO orchestration_v2_projection_runs VALUES
         ('r1', 't1', 1, 'codex', 'prov-1', 'completed', ?1, ?1, '{}'),
         ('r2', 't1', 2, 'codex', 'prov-1', 'running', ?1, NULL, '{}'),
         ('r3', 't2', 1, 'claudeAgent', 'prov-2', 'failed', ?1, ?1, '{}')",
        [now],
    )
    .unwrap();
    type Row<'a> = (&'a str, &'a str, Option<&'a str>, i64, &'a str, &'a str);
    let items: &[Row] = &[
        (
            "i1",
            "t1",
            Some("r1"),
            1,
            "user_message",
            r#"{"text":"why is CI red?"}"#,
        ),
        (
            "i2",
            "t1",
            Some("r1"),
            2,
            "reasoning",
            r#"{"text":"look at logs","streaming":false}"#,
        ),
        (
            "i3",
            "t1",
            Some("r1"),
            3,
            "command_execution",
            r#"{"input":"cargo test","output":"1 failed","exitCode":101,"outputIndicatesFailure":true}"#,
        ),
        (
            "i4",
            "t1",
            Some("r1"),
            4,
            "file_change",
            r#"{"fileName":"src/lib.rs"}"#,
        ),
        (
            "i5",
            "t1",
            Some("r1"),
            5,
            "todo_list",
            r#"{"steps":[{"id":"s1","text":"Reproduce","status":"completed"},{"id":"s2","text":"Fix","status":"running"}]}"#,
        ),
        (
            "i6",
            "t1",
            Some("r1"),
            6,
            "assistant_message",
            r#"{"text":"Fixed **it**.","streaming":false}"#,
        ),
        (
            "i7",
            "t1",
            Some("r2"),
            7,
            "user_message",
            r#"{"text":"thanks"}"#,
        ),
        ("i8", "t1", Some("r2"), 8, "checkpoint", r#"{"files":[]}"#),
        (
            "i9",
            "t2",
            Some("r3"),
            1,
            "user_message",
            r#"{"text":"hello"}"#,
        ),
        (
            "i10",
            "t2",
            Some("r3"),
            2,
            "error",
            r#"{"failure":{"class":"x","message":"rate limited"}}"#,
        ),
        (
            "i11",
            "t1",
            None,
            9,
            "system_notice",
            r#"{"message":"Switched provider"}"#,
        ),
    ];
    for (id, thread, run, ordinal, kind, payload) in items {
        conn.execute(
            "INSERT INTO orchestration_v2_projection_turn_items
             (turn_item_id, thread_id, run_id, ordinal, type, status, updated_at, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 'completed', ?6, ?7)",
            params![id, thread, run, ordinal, kind, now, payload],
        )
        .unwrap();
    }
    // Leave the last writes in the WAL (no checkpoint), like a running t3.
    std::mem::forget(conn);
    path
}

fn digest(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[test]
fn imports_projects_threads_runs_and_items_read_only() {
    let dir = scratch();
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let source_dir = dir.join("t3");
    std::fs::create_dir_all(&source_dir).unwrap();
    let source = synthetic_db(&source_dir, &workspace);
    let wal = PathBuf::from(format!("{}-wal", source.display()));
    let before: Vec<_> = std::fs::read_dir(&source_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let (db_before, wal_before) = (digest(&source), digest(&wal));

    let mut store = Store::open(&dir.join("blongo.sqlite")).unwrap();
    let report = import(&mut store, &source).unwrap();
    assert_eq!(
        report,
        ImportReport {
            projects: 1,
            threads: 2,
            runs: 3,
            items: 10,
            skipped_threads: 0,
        }
    );
    // The source is byte-for-byte unchanged and no files appeared next to
    // it (no -shm, no journal).
    assert_eq!(digest(&source), db_before);
    assert_eq!(digest(&wal), wal_before);
    let after: Vec<_> = std::fs::read_dir(&source_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after);

    let projects = store.projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "Demo");
    let threads = store.threads(true).unwrap();
    assert_eq!(threads.len(), 2);
    let fix = threads.iter().find(|t| t.title == "Fix the build").unwrap();
    assert_eq!(fix.provider, ProviderKind::Codex);
    assert!(!fix.archived);
    assert_eq!(fix.provider_thread_id, None);
    assert_eq!(fix.pending_context, Some(PendingContext::Handoff));
    let claude = threads.iter().find(|t| t.title == "Claude chat").unwrap();
    assert_eq!(claude.provider, ProviderKind::ClaudeCode);
    assert!(claude.archived);
    assert_eq!(store.threads(false).unwrap().len(), 1);

    let runs = store.runs(fix.id).unwrap();
    assert_eq!(
        runs.iter().map(|r| r.status).collect::<Vec<_>>(),
        vec![RunStatus::Completed, RunStatus::Interrupted]
    );
    let items = store.items(fix.id).unwrap();
    let kinds: Vec<&str> = items.iter().map(|i| i.kind.tag()).collect();
    assert_eq!(
        kinds,
        vec![
            "user_message",
            "reasoning",
            "command_execution",
            "file_change",
            "plan",
            "assistant_message",
            "user_message",
            "system_notice"
        ]
    );
    assert_eq!(&*items[0].text, "why is CI red?");
    assert_eq!(&*items[5].text, "Fixed **it**.");
    assert!(matches!(
        &items[2].kind,
        ItemKind::CommandExecution { command, exit_code: Some(101), output, .. }
            if command == "cargo test" && output == "1 failed"
    ));
    assert!(matches!(
        &items[4].kind,
        ItemKind::Plan { steps } if steps.len() == 2 && steps[1].status == PlanStatus::InProgress
    ));
    assert!(
        items
            .iter()
            .all(|i| i.run_id.is_some() || i.kind.tag() == "system_notice")
    );
    let claude_items = store.items(claude.id).unwrap();
    assert!(
        matches!(&claude_items[1].kind, ItemKind::Error { message } if message == "rate limited")
    );

    // Importing again adds nothing.
    let again = import(&mut store, &source).unwrap();
    assert_eq!(
        again,
        ImportReport {
            skipped_threads: 2,
            ..ImportReport::default()
        }
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rejects_files_that_are_not_t3code_databases() {
    let dir = scratch();
    let mut store = Store::open(&dir.join("blongo.sqlite")).unwrap();
    assert!(import(&mut store, &dir.join("missing.sqlite")).is_err());
    let other = dir.join("other.sqlite");
    Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE x (a)")
        .unwrap();
    let err = import(&mut store, &other).unwrap_err();
    assert!(err.to_string().contains("not a t3code"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
