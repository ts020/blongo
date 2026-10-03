//! Phase 4 end to end against the offline fake agents: diff and workspace
//! queries, git operations, scheduled runs, usage, the approval policy,
//! the agents' MCP tools (scope and delegation) and per-thread ordering of
//! work that now runs off the core loop.

mod common;

use blongo_core::{ApprovalPolicy, CoreSettings};
use blongo_protocol::workspace::{DiffScope, LineKind, Query, QueryReply};
use blongo_protocol::{Schedule, ScheduleId, Thread, Timestamp};
use common::*;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_project(dir: &Path) {
    let project = dir.join("project");
    git(&project, &["init", "--quiet", "-b", "main"]);
    git(&project, &["config", "user.name", "Test"]);
    git(&project, &["config", "user.email", "test@localhost"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.join("README.md"), "readme\n").unwrap();
    std::fs::write(project.join(".gitignore"), "*.log\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "init"]);
}

impl TestCore {
    async fn new_project(&mut self, path: &Path) -> ProjectId {
        let project_id = ProjectId::new();
        let c = self.dispatch(Command::ProjectCreate {
            project_id,
            name: String::new(),
            path: path.to_string_lossy().into_owned(),
        });
        self.ok(&c).await;
        project_id
    }

    async fn new_thread(&mut self, project_id: ProjectId) -> ThreadId {
        let thread_id = ThreadId::new();
        let c = self.dispatch(Command::ThreadCreate {
            thread_id,
            project_id,
            title: String::new(),
            provider: ProviderKind::Codex,
            model: None,
            worktree: false,
            parent_thread_id: None,
        });
        self.ok(&c).await;
        thread_id
    }

    async fn ok(&mut self, envelope: &CommandEnvelope) {
        let id = envelope.command_id;
        self.until(|e| match e {
            CoreEvent::Event(ev) if ev.command_id == Some(id) => Some(()),
            CoreEvent::CommandRejected { command_id, reason } if *command_id == id => {
                panic!("rejected: {reason}")
            }
            _ => None,
        })
        .await
    }

    async fn turn(&mut self, thread_id: ThreadId, text: &str) -> RunStatus {
        self.send(thread_id, text);
        self.run_finished().await
    }

    async fn query(&mut self, query: Query) -> Result<QueryReply, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.handle().client().query(id, query);
        self.until(|e| match e {
            CoreEvent::Reply { id: got, result } if *got == id => Some(result.clone()),
            _ => None,
        })
        .await
    }

    async fn shell(&mut self) -> Arc<ShellSnapshot> {
        self.handle().client().shell();
        self.until(|e| match e {
            CoreEvent::Shell(s) => Some(s.clone()),
            _ => None,
        })
        .await
    }

    /// The last assistant message of the thread.
    async fn last_answer(&mut self, thread_id: ThreadId) -> String {
        let snap = self.snapshot(thread_id).await;
        texts(&snap, "assistant_message").pop().unwrap_or_default()
    }
}

#[tokio::test]
async fn turn_and_thread_diffs_from_checkpoints() {
    let dir = temp_dir("p4-diff");
    git_project(&dir);
    let (mut core, _) = TestCore::start(&dir);
    let project = core.new_project(&dir.join("project")).await;
    let thread = core.new_thread(project).await;
    assert_eq!(
        core.turn(thread, "write a.txt one").await,
        RunStatus::Completed
    );
    assert_eq!(
        core.turn(thread, "write b.txt two").await,
        RunStatus::Completed
    );
    let runs = core.snapshot(thread).await.runs.clone();
    assert!(runs.iter().all(|r| r.checkpoint.is_some()), "{runs:?}");

    // Turn 1: its checkpoint → turn 2's checkpoint.
    let Ok(QueryReply::DiffSummary(first)) = core
        .query(Query::DiffSummary {
            thread_id: thread,
            scope: DiffScope::Turn { run_id: runs[0].id },
        })
        .await
    else {
        panic!("no summary")
    };
    let paths: Vec<&str> = first.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.txt"]);
    assert_eq!(first.files[0].status, 'A');
    assert_eq!((first.added, first.removed), (1, 0));
    // The latest turn ends at the working tree.
    let Ok(QueryReply::DiffSummary(second)) = core
        .query(Query::DiffSummary {
            thread_id: thread,
            scope: DiffScope::Turn { run_id: runs[1].id },
        })
        .await
    else {
        panic!("no summary")
    };
    assert_eq!(second.files.len(), 1);
    assert_eq!(second.files[0].path, "b.txt");
    // The whole thread.
    let Ok(QueryReply::DiffSummary(all)) = core
        .query(Query::DiffSummary {
            thread_id: thread,
            scope: DiffScope::Thread,
        })
        .await
    else {
        panic!("no summary")
    };
    assert_eq!(all.files.len(), 2);
    // One file's hunks.
    let Ok(QueryReply::DiffFile(file)) = core
        .query(Query::DiffFile {
            thread_id: thread,
            from: all.from.clone(),
            to: all.to.clone(),
            path: "a.txt".into(),
            max_lines: 100,
        })
        .await
    else {
        panic!("no file diff")
    };
    let added: Vec<&str> = file.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(added, ["one"]);
    // Trees are object ids, never revision expressions or options.
    let err = core
        .query(Query::DiffFile {
            thread_id: thread,
            from: "--output=/tmp/x".into(),
            to: all.to.clone(),
            path: "a.txt".into(),
            max_lines: 100,
        })
        .await
        .unwrap_err();
    assert!(err.contains("not a diff"), "{err}");
    let err = core
        .query(Query::DiffFile {
            thread_id: thread,
            from: all.from.clone(),
            to: all.to.clone(),
            path: "../outside".into(),
            max_lines: 100,
        })
        .await
        .unwrap_err();
    assert!(!err.is_empty());
    core.shutdown();
}

#[tokio::test]
async fn files_search_and_git_operations() {
    let dir = temp_dir("p4-files");
    git_project(&dir);
    let repo = dir.join("project");
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    std::fs::write(repo.join("src/deep/main_window.rs"), "fn main() {}\n").unwrap();
    std::fs::write(repo.join("debug.log"), "ignored\n").unwrap();
    let (mut core, _) = TestCore::start(&dir);
    let project = core.new_project(&repo).await;
    let thread = core.new_thread(project).await;

    let Ok(QueryReply::Files(hits)) = core
        .query(Query::SearchFiles {
            thread_id: thread,
            pattern: "mainwin".into(),
            limit: 10,
        })
        .await
    else {
        panic!("no search")
    };
    assert_eq!(hits[0].path, "src/deep/main_window.rs");
    // .gitignore is respected.
    let Ok(QueryReply::Files(hits)) = core
        .query(Query::SearchFiles {
            thread_id: thread,
            pattern: "debug".into(),
            limit: 10,
        })
        .await
    else {
        panic!("no search")
    };
    assert!(hits.is_empty(), "{hits:?}");
    let Ok(QueryReply::Dir(entries)) = core
        .query(Query::ListDir {
            thread_id: thread,
            path: String::new(),
        })
        .await
    else {
        panic!("no listing")
    };
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"src") && names.contains(&"README.md"),
        "{names:?}"
    );
    assert!(
        !names.contains(&"debug.log") && !names.contains(&".git"),
        "{names:?}"
    );
    let Ok(QueryReply::File(file)) = core
        .query(Query::ReadFile {
            thread_id: thread,
            path: "README.md".into(),
            max_bytes: 1000,
        })
        .await
    else {
        panic!("no file")
    };
    assert_eq!(file.text, "readme\n");
    assert!(
        core.query(Query::ReadFile {
            thread_id: thread,
            path: "../../etc/passwd".into(),
            max_bytes: 1000,
        })
        .await
        .is_err()
    );

    // Status, branch create + switch, commit.
    let Ok(QueryReply::GitStatus(status)) =
        core.query(Query::GitStatus { thread_id: thread }).await
    else {
        panic!("no status")
    };
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.changes.len(), 1, "{status:?}"); // src/ (untracked)
    let reply = core
        .query(Query::GitSwitch {
            thread_id: thread,
            branch: "feature/x".into(),
            create: true,
        })
        .await
        .unwrap();
    assert!(matches!(reply, QueryReply::Done(_)));
    assert_eq!(git(&repo, &["branch", "--show-current"]), "feature/x");
    assert!(
        core.query(Query::GitSwitch {
            thread_id: thread,
            branch: "-bad".into(),
            create: true,
        })
        .await
        .is_err()
    );
    core.query(Query::GitCommit {
        thread_id: thread,
        message: "add main window".into(),
    })
    .await
    .unwrap();
    assert_eq!(git(&repo, &["log", "-1", "--format=%s"]), "add main window");
    let Ok(QueryReply::Branches(branches)) =
        core.query(Query::GitBranches { thread_id: thread }).await
    else {
        panic!("no branches")
    };
    assert!(branches.iter().any(|b| b.name == "feature/x" && b.current));
    assert!(branches.iter().any(|b| b.name == "main" && !b.current));

    // Not while the thread runs.
    core.send(thread, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let err = core
        .query(Query::GitSwitch {
            thread_id: thread,
            branch: "main".into(),
            create: false,
        })
        .await
        .unwrap_err();
    assert!(err.contains("wait"), "{err}");
    core.dispatch(Command::RunInterrupt { thread_id: thread });
    core.run_finished().await;
    core.shutdown();
}

#[tokio::test]
async fn work_for_one_thread_keeps_its_order_while_checkpoints_run_off_the_loop() {
    let dir = temp_dir("p4-order");
    git_project(&dir);
    let (mut core, _) = TestCore::start(&dir);
    let project = core.new_project(&dir.join("project")).await;
    let thread = core.new_thread(project).await;
    let other = core.new_thread(project).await;
    // Sent back to back: the second is queued behind the first even though
    // the first's checkpoint is still being taken off the loop.
    core.send(thread, "write a.txt one");
    core.send(thread, "echo: second");
    // Another thread's work is not held up meanwhile.
    core.send(other, "echo: other");
    let mut finished = 0;
    let mut checkpointed_before_text = true;
    let mut seen_checkpoints = std::collections::HashSet::new();
    while finished < 3 {
        match core.next().await {
            CoreEvent::RunFinished { status, .. } => {
                assert_eq!(status, RunStatus::Completed);
                finished += 1;
            }
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::RunCheckpointed { run_id, .. } => {
                    seen_checkpoints.insert(*run_id);
                }
                EventKind::ItemAdded { item }
                    if matches!(item.kind, ItemKind::AssistantMessage { .. }) =>
                {
                    checkpointed_before_text &=
                        item.run_id.is_some_and(|r| seen_checkpoints.contains(&r));
                }
                _ => {}
            },
            _ => {}
        }
    }
    assert!(
        checkpointed_before_text,
        "a run streamed before its checkpoint"
    );
    let snap = core.snapshot(thread).await;
    assert_eq!(
        texts(&snap, "user_message"),
        ["write a.txt one", "echo: second"]
    );
    assert_eq!(snap.runs.len(), 2);
    assert!(snap.runs.iter().all(|r| r.checkpoint.is_some()));
    core.shutdown();
}

#[tokio::test]
async fn usage_is_recorded_per_turn() {
    let dir = temp_dir("p4-usage");
    let (mut core, _) = TestCore::start(&dir);
    let (_, thread) = core.project_and_thread(&dir).await;
    core.send(thread, "usage");
    let usage = core
        .until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::RunUsage { usage, .. } if usage.input_tokens == 300 => Some(*usage),
                _ => None,
            },
            _ => None,
        })
        .await;
    assert_eq!((usage.output_tokens, usage.cached_input_tokens), (21, 20));
    core.run_finished().await;
    let snap = core.snapshot(thread).await;
    assert_eq!(snap.runs[0].usage, Some(usage));
    core.shutdown();
}

#[tokio::test]
async fn auto_approve_answers_without_waiting() {
    let dir = temp_dir("p4-approve");
    let (mut core, _) = TestCore::start(&dir);
    let (_, thread) = core.project_and_thread(&dir).await;
    core.handle().client().configure(CoreSettings {
        approval: ApprovalPolicy::AutoApprove,
        ..CoreSettings::default()
    });
    core.send(thread, "run a tool");
    let mut waited = false;
    let status = loop {
        match core.next().await {
            CoreEvent::Event(ev) => {
                if let EventKind::RunStatusChanged {
                    status: RunStatus::Waiting,
                    ..
                } = ev.kind
                {
                    waited = true;
                }
            }
            CoreEvent::RunFinished { status, .. } => break status,
            _ => {}
        }
    };
    assert_eq!(status, RunStatus::Completed);
    assert!(!waited);
    let snap = core.snapshot(thread).await;
    assert!(snap.items.iter().any(|i| matches!(
        i.kind,
        ItemKind::ApprovalRequest {
            state: ApprovalState::Approved,
            ..
        }
    )));
    core.shutdown();
}

#[tokio::test]
async fn schedules_fire_persist_and_catch_up_after_a_restart() {
    let dir = temp_dir("p4-schedule");
    let (mut core, _) = TestCore::start(&dir);
    let (project, thread) = core.project_and_thread(&dir).await;
    // Bad expressions are refused.
    let c = core.dispatch(Command::ScheduleCreate {
        schedule_id: ScheduleId::new(),
        project_id: project,
        thread_id: None,
        cron: "61 * * * *".into(),
        prompt: "x".into(),
        provider: ProviderKind::Codex,
        proposed_by: None,
    });
    assert!(core.rejected(&c).await.contains("out of range"));

    // A schedule posting into a new thread each time, run by hand.
    let to_new = ScheduleId::new();
    let c = core.dispatch(Command::ScheduleCreate {
        schedule_id: to_new,
        project_id: project,
        thread_id: None,
        cron: "0 9 * * 1-5".into(),
        prompt: "echo: scheduled".into(),
        provider: ProviderKind::Codex,
        proposed_by: None,
    });
    core.ok(&c).await;
    core.dispatch(Command::ScheduleRunNow {
        schedule_id: to_new,
    });
    let created = core
        .until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } => Some(thread.clone()),
                _ => None,
            },
            _ => None,
        })
        .await;
    assert!(
        created.title.starts_with("Scheduled: "),
        "{}",
        created.title
    );
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    assert_eq!(core.last_answer(created.id).await, "echo: scheduled");

    // One bound to a thread.
    let bound = ScheduleId::new();
    let c = core.dispatch(Command::ScheduleCreate {
        schedule_id: bound,
        project_id: project,
        thread_id: Some(thread),
        cron: "@daily".into(),
        prompt: "echo: daily".into(),
        provider: ProviderKind::Codex,
        proposed_by: None,
    });
    core.ok(&c).await;
    core.shutdown();

    // While Blongo was not running its time passed: it runs once at start.
    let db = dir.join("data/blongo.sqlite");
    {
        let mut store = blongo_core::Store::open(&db).unwrap();
        let mut schedule: Schedule = store
            .schedules()
            .unwrap()
            .into_iter()
            .find(|s| s.id == bound)
            .unwrap();
        schedule.next_run_at = Some(Timestamp(Timestamp::now().0 - 3_600_000));
        store
            .commit(blongo_store::Batch {
                command_id: None,
                events: vec![EventKind::ScheduleUpdated { schedule }],
                effects: vec![],
            })
            .unwrap();
    }
    let (mut core, shell) = TestCore::start(&dir);
    assert_eq!(shell.schedules.len(), 2);
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    assert_eq!(core.last_answer(thread).await, "echo: daily");
    let shell = {
        core.handle().client().shell();
        core.until(|e| match e {
            CoreEvent::Shell(s) => Some(s.clone()),
            _ => None,
        })
        .await
    };
    let s = shell.schedules.iter().find(|s| s.id == bound).unwrap();
    assert!(s.next_run_at.unwrap() > Timestamp::now());
    assert_eq!(s.last_thread_id, Some(thread));
    // Disabled: no next time; deleted: gone.
    let c = core.dispatch(Command::ScheduleUpdate {
        schedule_id: bound,
        enabled: Some(false),
        cron: None,
        prompt: None,
    });
    core.ok(&c).await;
    let c = core.dispatch(Command::ScheduleDelete {
        schedule_id: to_new,
    });
    core.ok(&c).await;
    core.shutdown();
    let store = blongo_core::Store::open(&db).unwrap();
    let left = store.schedules().unwrap();
    assert_eq!(left.len(), 1);
    assert!(!left[0].enabled && left[0].next_run_at.is_none());
}

fn start_mcp(dir: &Path) -> (TestCore, Arc<ShellSnapshot>) {
    let dump = dir.join("starts.jsonl");
    TestCore::start_with(dir, |c| {
        c.mcp_bridge = Some((
            PathBuf::from("python3"),
            vec![fixture("fake_mcp_bridge.py").to_string_lossy().into_owned()],
        ));
        c.agent_env.push((
            "FAKE_CODEX_DUMP_START".into(),
            dump.to_string_lossy().into_owned(),
        ));
    })
}

#[tokio::test]
async fn mcp_tools_see_only_their_project_and_delegate_to_children() {
    let dir = temp_dir("p4-mcp");
    std::fs::create_dir_all(dir.join("other")).unwrap();
    let (mut core, _) = start_mcp(&dir);
    let project = core.new_project(&dir.join("project")).await;
    let caller = core.new_thread(project).await;
    let sibling = core.new_thread(project).await;
    let elsewhere_project = core.new_project(&dir.join("other")).await;
    let elsewhere = core.new_thread(elsewhere_project).await;

    core.turn(caller, "mcp: t3_thread_list {}").await;
    let listed = core.last_answer(caller).await;
    assert!(listed.contains(&caller.to_string()), "{listed}");
    assert!(listed.contains(&sibling.to_string()), "{listed}");
    assert!(!listed.contains(&elsewhere.to_string()), "{listed}");
    // The session got the bridge in its MCP config (socket + token file,
    // never the token itself).
    let starts = std::fs::read_to_string(dir.join("starts.jsonl")).unwrap();
    let start: serde_json::Value = serde_json::from_str(starts.lines().next().unwrap()).unwrap();
    let args = start["config"]["mcp_servers"]["blongo"]["args"]
        .as_array()
        .unwrap()
        .clone();
    assert!(args.last().unwrap().as_str().unwrap().ends_with(".token"));

    // Another project's thread does not exist for the caller.
    core.turn(
        caller,
        &format!("mcp: t3_thread_read {{\"threadId\": \"{elsewhere}\"}}"),
    )
    .await;
    let answer = core.last_answer(caller).await;
    assert!(answer.starts_with("ERROR unknown thread"), "{answer}");
    core.turn(
        caller,
        &format!("mcp: t3_thread_send {{\"threadId\": \"{elsewhere}\", \"message\": \"hi\"}}"),
    )
    .await;
    assert!(core.last_answer(caller).await.starts_with("ERROR"));
    assert!(core.snapshot(elsewhere).await.items.is_empty());

    // Delegation: a child thread runs the task and the caller gets its
    // final message.
    core.send(
        caller,
        "mcp: delegate_task {\"prompt\": \"echo: child work\", \"title\": \"Child\"}",
    );
    let child = core
        .until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } => Some(thread.clone()),
                _ => None,
            },
            CoreEvent::TextDelta { chunk, .. } => panic!("answered: {chunk}"),
            _ => None,
        })
        .await;
    assert_eq!(child.parent_thread_id, Some(caller));
    assert_eq!(child.title, "Child");
    // The child's run, then the caller's.
    core.run_finished().await;
    core.run_finished().await;
    let answer = core.last_answer(caller).await;
    assert!(answer.contains("echo: child work"), "{answer}");
    assert!(answer.contains(&child.id.to_string()), "{answer}");

    // Async mode returns at once; task_status reads it later; only the
    // parent may ask.
    core.send(
        caller,
        "mcp: delegate_task {\"prompt\": \"echo: later\", \"mode\": \"async\"}",
    );
    let mut runs = 0;
    while runs < 2 {
        if let CoreEvent::RunFinished { .. } = core.next().await {
            runs += 1;
        }
    }
    let answer = core.last_answer(caller).await;
    let v: serde_json::Value = serde_json::from_str(&answer).unwrap();
    let child2 = v["childThreadId"].as_str().unwrap().to_owned();
    core.turn(
        caller,
        &format!("mcp: task_status {{\"childThreadId\": \"{child2}\"}}"),
    )
    .await;
    let status = core.last_answer(caller).await;
    assert!(status.contains("echo: later"), "{status}");
    core.turn(
        sibling,
        &format!("mcp: task_status {{\"childThreadId\": \"{child2}\"}}"),
    )
    .await;
    assert!(core.last_answer(sibling).await.starts_with("ERROR"));
    core.shutdown();
}

/// Collect events until `runs` runs finished; returns the threads created
/// meanwhile.
async fn threads_created_until_runs(core: &mut TestCore, runs: usize) -> Vec<Thread> {
    let mut created = Vec::new();
    let mut finished = 0;
    while finished < runs {
        match core.next().await {
            CoreEvent::Event(ev) => {
                if let EventKind::ThreadCreated { thread } = &ev.kind {
                    created.push(thread.clone());
                }
            }
            CoreEvent::RunFinished { .. } => finished += 1,
            _ => {}
        }
    }
    created
}

#[tokio::test]
async fn agents_cannot_escape_the_spawn_limits() {
    let dir = temp_dir("p4-mcp-limits");
    std::fs::create_dir_all(dir.join("project")).unwrap();
    let (mut core, _) = start_mcp(&dir);
    let project = core.new_project(&dir.join("project")).await;
    let caller = core.new_thread(project).await;

    // 1. t3_thread_create is no way around the depth limit: the threads
    // it makes are the caller's children, and a grandchild may not start
    // another thread.
    let l3 = "mcp: t3_thread_create {}";
    let l2 = format!(
        "mcp: t3_thread_create {}",
        serde_json::json!({ "message": l3 })
    );
    let l1 = format!(
        "mcp: t3_thread_create {}",
        serde_json::json!({ "message": l2 })
    );
    core.send(caller, &l1);
    let created = threads_created_until_runs(&mut core, 3).await;
    assert_eq!(created.len(), 2, "{created:?}");
    let (c1, c2) = (&created[0], &created[1]);
    assert_eq!(c1.parent_thread_id, Some(caller));
    assert_eq!(c2.parent_thread_id, Some(c1.id));
    let refused = core.last_answer(c2.id).await;
    assert!(
        refused.starts_with("ERROR threads started by agents are limited to 2 levels"),
        "{refused}"
    );
    // Delegation from there is refused the same way.
    core.turn(c2.id, "mcp: delegate_task {\"prompt\": \"echo: deeper\"}")
        .await;
    assert!(
        core.last_answer(c2.id)
            .await
            .starts_with("ERROR threads started by agents")
    );

    // 2. Concurrency counts creations still waiting in line: while a
    // global job (a branch switch whose hook takes seconds) holds the line,
    // six simultaneous delegations get four children and two refusals.
    let other = dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "--quiet", "-b", "main"]);
    git(&other, &["config", "user.name", "Test"]);
    git(&other, &["config", "user.email", "test@localhost"]);
    git(&other, &["config", "commit.gpgsign", "false"]);
    std::fs::write(other.join("a"), "a\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "--quiet", "-m", "init"]);
    git(&other, &["branch", "b2"]);
    let hook = other.join(".git/hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\nsleep 4\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let other_project = core.new_project(&other).await;
    let other_thread = core.new_thread(other_project).await;
    let spawner = core.new_thread(project).await;
    core.send(
        spawner,
        "mcp*6 after=1.5: delegate_task {\"prompt\": \"echo: job\", \"mode\": \"async\"}",
    );
    core.handle().client().query(
        7_000_001,
        Query::GitSwitch {
            thread_id: other_thread,
            branch: "b2".into(),
            create: false,
        },
    );
    let mut switched = false;
    loop {
        match core.next().await {
            CoreEvent::Reply {
                id: 7_000_001,
                result,
            } => {
                assert!(result.is_ok(), "{result:?}");
                switched = true;
            }
            CoreEvent::RunFinished { thread_id, .. } if thread_id == spawner => break,
            _ => {}
        }
    }
    // The accepted calls were answered only after the global job.
    assert!(switched);
    let answer = core.last_answer(spawner).await;
    let started = answer.matches("\"childThreadId\"").count();
    let refused = answer
        .matches("ERROR 4 threads this thread started are still working")
        .count();
    assert_eq!((started, refused), (4, 2), "{answer}");
    // Let the four children finish.
    threads_created_until_runs(&mut core, 4).await;

    // 3. Schedules from agents: too frequent ones are refused, the rest
    // wait disabled until the user turns them on.
    core.turn(
        caller,
        "mcp: schedule_task {\"cron\": \"* * * * *\", \"prompt\": \"echo: tick\"}",
    )
    .await;
    let answer = core.last_answer(caller).await;
    assert!(
        answer.starts_with("ERROR `* * * * *` runs more often than every 15 minutes"),
        "{answer}"
    );
    core.turn(
        caller,
        "mcp: schedule_task {\"cron\": \"*/30 * * * *\", \"prompt\": \"echo: tick\", \"bindToCurrentThread\": false}",
    )
    .await;
    let answer = core.last_answer(caller).await;
    let v: serde_json::Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(v["enabled"], false, "{answer}");
    let schedule_id = ScheduleId::parse(v["scheduleId"].as_str().unwrap()).unwrap();
    let shell = core.shell().await;
    let proposal = shell
        .schedules
        .iter()
        .find(|s| s.id == schedule_id)
        .unwrap()
        .clone();
    assert!(!proposal.enabled);
    assert_eq!(proposal.next_run_at, None);
    assert_eq!(proposal.proposed_by, Some(caller));
    // The user's approval: turning it on.
    let c = core.dispatch(Command::ScheduleUpdate {
        schedule_id,
        enabled: Some(true),
        cron: None,
        prompt: None,
    });
    core.ok(&c).await;
    let shell = core.shell().await;
    let approved = shell
        .schedules
        .iter()
        .find(|s| s.id == schedule_id)
        .unwrap();
    assert!(approved.enabled && approved.next_run_at.is_some());
    assert_eq!(approved.proposed_by, None);

    // 4. A session's token dies with the session: once the thread is
    // archived, its old token no longer opens the MCP socket.
    let starts = std::fs::read_to_string(dir.join("starts.jsonl")).unwrap();
    let start: serde_json::Value = serde_json::from_str(starts.lines().next().unwrap()).unwrap();
    let args = start["config"]["mcp_servers"]["blongo"]["args"]
        .as_array()
        .unwrap()
        .clone();
    let socket = PathBuf::from(args[1].as_str().unwrap());
    let token_file = PathBuf::from(args[2].as_str().unwrap());
    let token = std::fs::read_to_string(&token_file).unwrap();
    let c = core.dispatch(Command::ThreadArchive { thread_id: caller });
    core.ok(&c).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!token_file.exists(), "the token file outlived the session");
    let reply = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"t3_thread_list","arguments":{}}}"#;
        let _ = write!(s, "blongo-mcp {}\n{call}\n", token.trim());
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    })
    .await
    .unwrap();
    assert!(
        !reply.contains("threads"),
        "a revoked token was served: {reply}"
    );
    core.shutdown();
}
