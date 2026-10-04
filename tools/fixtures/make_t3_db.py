#!/usr/bin/env python3
"""Write a small synthetic t3code statev2.sqlite (for the import e2e).

Usage: make_t3_db.py OUT.sqlite WORKSPACE_DIR

Same tables/columns as crates/blongo-core/tests/t3_import.rs (t3code
migrations 005_Projections and 055_OrchestrationV2). Never touches ~/.t3.
"""
import sqlite3
import sys

SCHEMA = """
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
"""

out, workspace = sys.argv[1], sys.argv[2]
now = "2026-05-01T12:00:00.000Z"
db = sqlite3.connect(out)
db.executescript(SCHEMA)
db.execute("INSERT INTO projection_projects VALUES ('p1','t3 project',?,NULL,'[]',?,?,NULL)",
           (workspace, now, now))
db.execute("INSERT INTO orchestration_v2_projection_threads VALUES "
           "('t1','p1','Imported from t3code','claudeAgent','full-access','default','prov-1',?,?,NULL,NULL,'{}')",
           (now, now))
db.execute("INSERT INTO orchestration_v2_projection_runs VALUES "
           "('r1','t1',1,'claudeAgent','prov-1','completed',?,?,'{}')", (now, now))
items = [
    ("i1", 1, "user_message", '{"text":"what changed in t3code?"}'),
    ("i2", 2, "todo_list", '{"steps":[{"id":"a","text":"Read the log","status":"completed"},'
                           '{"id":"b","text":"Summarize","status":"completed"}]}'),
    ("i3", 3, "assistant_message", '{"text":"Two things:\\n\\n- **imports** work\\n- plans too",'
                                   '"streaming":false}'),
]
for iid, ordinal, kind, payload in items:
    db.execute("INSERT INTO orchestration_v2_projection_turn_items VALUES "
               "(?, 't1', 'r1', NULL, 'prov-1', NULL, NULL, ?, ?, 'completed', ?, ?)",
               (iid, ordinal, kind, now, payload))
db.commit()
db.close()
