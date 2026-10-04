//! Forward-only schema migrations. Each step runs in its own transaction and
//! records its version in `schema_migrations`.

use anyhow::bail;
use rusqlite::{Connection, params};

const MIGRATIONS: &[(u32, &str)] = &[
    (
        1,
        r#"
CREATE TABLE events (
    sequence   INTEGER PRIMARY KEY,
    at         INTEGER NOT NULL,
    thread_id  TEXT,
    command_id TEXT,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX events_thread ON events (thread_id, sequence);

CREATE TABLE command_receipts (
    command_id     TEXT PRIMARY KEY,
    first_sequence INTEGER,
    last_sequence  INTEGER,
    at             INTEGER NOT NULL
);

CREATE TABLE projects (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    path       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE threads (
    id                 TEXT PRIMARY KEY,
    project_id         TEXT NOT NULL REFERENCES projects (id),
    title              TEXT NOT NULL,
    status             TEXT NOT NULL,
    archived           INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    provider_thread_id TEXT
);
CREATE INDEX threads_project ON threads (project_id);

CREATE TABLE runs (
    id            TEXT PRIMARY KEY,
    thread_id     TEXT NOT NULL REFERENCES threads (id),
    parent_run_id TEXT REFERENCES runs (id),
    status        TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    ended_at      INTEGER,
    error         TEXT
);
CREATE INDEX runs_thread ON runs (thread_id, created_at);
CREATE INDEX runs_status ON runs (status);

CREATE TABLE turn_items (
    id         TEXT PRIMARY KEY,
    thread_id  TEXT NOT NULL REFERENCES threads (id),
    run_id     TEXT REFERENCES runs (id),
    ordinal    INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    data       TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    UNIQUE (thread_id, ordinal)
);
CREATE INDEX turn_items_run ON turn_items (run_id);

CREATE TABLE effect_outbox (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id  TEXT NOT NULL,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL,
    status     TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX effect_outbox_status ON effect_outbox (status, id);
"#,
    ),
    (
        2,
        r#"
ALTER TABLE threads ADD COLUMN provider TEXT NOT NULL DEFAULT 'codex';
ALTER TABLE threads ADD COLUMN model TEXT;
ALTER TABLE threads ADD COLUMN worktree_path TEXT;
ALTER TABLE threads ADD COLUMN worktree_branch TEXT;
ALTER TABLE threads ADD COLUMN forked_from TEXT;
ALTER TABLE threads ADD COLUMN pending_context TEXT;

ALTER TABLE runs ADD COLUMN provider TEXT NOT NULL DEFAULT 'codex';
ALTER TABLE runs ADD COLUMN provider_turn_id TEXT;
ALTER TABLE runs ADD COLUMN checkpoint TEXT;
"#,
    ),
    (
        3,
        r#"
ALTER TABLE threads ADD COLUMN parent_thread_id TEXT;
ALTER TABLE runs ADD COLUMN usage TEXT;

CREATE TABLE schedules (
    id             TEXT PRIMARY KEY,
    project_id     TEXT NOT NULL REFERENCES projects (id),
    thread_id      TEXT,
    cron           TEXT NOT NULL,
    prompt         TEXT NOT NULL,
    provider       TEXT NOT NULL DEFAULT 'codex',
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     INTEGER NOT NULL,
    next_run_at    INTEGER,
    last_run_at    INTEGER,
    last_thread_id TEXT
);
"#,
    ),
    (
        4,
        r#"
ALTER TABLE schedules ADD COLUMN proposed_by TEXT;
"#,
    ),
    (
        5,
        r#"
ALTER TABLE threads ADD COLUMN pr TEXT;
ALTER TABLE threads ADD COLUMN pr_status TEXT;
ALTER TABLE threads ADD COLUMN pr_dismissed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE projects ADD COLUMN forge TEXT;

-- What Blongo last learned from the forge (a repository's default
-- branch, …): a cache, not part of the event log.
CREATE TABLE forge_cache (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    at    INTEGER NOT NULL
);
"#,
    ),
];

pub const LATEST_VERSION: u32 = MIGRATIONS[MIGRATIONS.len() - 1].0;

pub(crate) fn current_version(conn: &Connection) -> anyhow::Result<u32> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn migrate(conn: &Connection) -> anyhow::Result<()> {
    migrate_with(conn, MIGRATIONS)
}

pub(crate) fn migrate_with(conn: &Connection, steps: &[(u32, &str)]) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version    INTEGER PRIMARY KEY,
             applied_at INTEGER NOT NULL
         );",
    )?;
    let current = current_version(conn)?;
    let latest = steps.last().map_or(0, |s| s.0);
    if current > latest {
        bail!("database schema v{current} is newer than this build (v{latest})");
    }
    for (version, sql) in steps.iter().filter(|(v, _)| *v > current) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, blongo_protocol::Timestamp::now().0],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_incremental_and_refuse_newer_schemas() {
        let conn = Connection::open_in_memory().unwrap();
        let v1 = &MIGRATIONS[..1];
        migrate_with(&conn, v1).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 1);
        // Re-running applies nothing (would fail on CREATE TABLE otherwise).
        migrate_with(&conn, v1).unwrap();
        // A later step applies on top, once.
        let v2 = [
            MIGRATIONS[0],
            (
                2,
                "ALTER TABLE projects ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;",
            ),
        ];
        migrate_with(&conn, &v2).unwrap();
        migrate_with(&conn, &v2).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 2);
        conn.execute("UPDATE projects SET pinned = 1", []).unwrap();
        // An older build refuses a newer database instead of corrupting it.
        let err = migrate_with(&conn, v1).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[test]
    fn v1_data_survives_the_provider_migration() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_with(&conn, &MIGRATIONS[..1]).unwrap();
        conn.execute_batch(
            "INSERT INTO projects VALUES ('p', 'demo', '/tmp', 1);
             INSERT INTO threads (id, project_id, title, status, archived, created_at,
                                  updated_at, provider_thread_id)
             VALUES ('t', 'p', 'old', 'idle', 0, 1, 1, 'codex-thread');
             INSERT INTO runs (id, thread_id, status, created_at)
             VALUES ('r', 't', 'completed', 1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST_VERSION);
        let (provider, model): (String, Option<String>) = conn
            .query_row("SELECT provider, model FROM threads", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((provider.as_str(), model), ("codex", None));
        let run_provider: String = conn
            .query_row("SELECT provider FROM runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(run_provider, "codex");
    }

    #[test]
    fn failed_step_rolls_back() {
        let conn = Connection::open_in_memory().unwrap();
        let bad = [(1, "CREATE TABLE a (x INTEGER); THIS IS NOT SQL;")];
        assert!(migrate_with(&conn, &bad).is_err());
        assert_eq!(current_version(&conn).unwrap(), 0);
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0);
    }
}
