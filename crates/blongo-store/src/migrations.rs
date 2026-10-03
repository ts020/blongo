//! Forward-only schema migrations. Each step runs in its own transaction and
//! records its version in `schema_migrations`.

use anyhow::bail;
use rusqlite::{Connection, params};

const MIGRATIONS: &[(u32, &str)] = &[(
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
)];

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
