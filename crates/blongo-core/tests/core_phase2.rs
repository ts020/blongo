//! Phase 2 orchestration end to end against the offline fake agents
//! (crates/blongo-harness/tests/fixtures/fake_{codex,claude,acp}.py):
//! Claude Code and Antigravity threads, queue and steer, fork, provider
//! switch with context handoff, checkpoints and rollback, worktrees, plans,
//! models, sign-in and install.

mod common;

use blongo_protocol::{PendingContext, PlanStatus};
use common::*;

fn start(dir: &Path) -> TestCore {
    start_tweaked(dir, |_| {})
}

fn start_tweaked(dir: &Path, tweak: impl FnOnce(&mut CoreConfig)) -> TestCore {
    TestCore::start_with(dir, |c| {
        c.claude_executable = Some(fixture("fake_claude.py"));
        c.antigravity_executable = Some(fixture("fake_acp.py"));
        tweak(c);
    })
    .0
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Make `<dir>/project` a git repository with one commit.
fn git_project(dir: &Path) {
    let project = dir.join("project");
    git(&project, &["init", "--quiet", "-b", "main"]);
    git(&project, &["config", "user.name", "Test"]);
    git(&project, &["config", "user.email", "test@localhost"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    std::fs::write(project.join("README.md"), "readme\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "init"]);
}

impl TestCore {
    async fn project(&mut self, dir: &Path) -> ProjectId {
        let project_id = ProjectId::new();
        self.dispatch(Command::ProjectCreate {
            project_id,
            name: String::new(),
            path: dir.join("project").to_string_lossy().into_owned(),
        });
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ProjectCreated { project } if project.id == project_id => Some(()),
                _ => None,
            },
            _ => None,
        })
        .await;
        project_id
    }

    async fn thread(
        &mut self,
        project_id: ProjectId,
        provider: ProviderKind,
        worktree: bool,
    ) -> blongo_protocol::Thread {
        let thread_id = ThreadId::new();
        self.dispatch(Command::ThreadCreate {
            thread_id,
            project_id,
            title: String::new(),
            provider,
            model: None,
            worktree,
        });
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } if thread.id == thread_id => {
                    Some(thread.clone())
                }
                _ => None,
            },
            CoreEvent::CommandRejected { reason, .. } => panic!("rejected: {reason}"),
            _ => None,
        })
        .await
    }

    /// Send, then wait for the run to finish; returns its status.
    async fn turn(&mut self, thread_id: ThreadId, text: &str) -> RunStatus {
        self.send(thread_id, text);
        self.run_finished().await
    }

    /// Approve every approval request until the run finishes.
    async fn turn_approving(&mut self, thread_id: ThreadId, text: &str) -> RunStatus {
        self.send(thread_id, text);
        loop {
            match self.next().await {
                CoreEvent::Event(ev) => {
                    if let EventKind::ItemAdded { item } = &ev.kind
                        && matches!(item.kind, ItemKind::ApprovalRequest { .. })
                    {
                        self.dispatch(Command::RuntimeRequestRespond {
                            thread_id,
                            item_id: item.id,
                            decision: ApprovalDecision::Approve,
                        });
                    }
                }
                CoreEvent::RunFinished { status, .. } => return status,
                _ => {}
            }
        }
    }

    async fn accepted(&mut self, envelope: &CommandEnvelope) {
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

    /// The thread as stored (read through a second connection after a
    /// snapshot round trip, so every earlier commit is visible).
    async fn thread_state(&mut self, thread_id: ThreadId) -> blongo_protocol::Thread {
        self.snapshot(thread_id).await;
        let db = self.db_path.clone().unwrap();
        blongo_core::Store::open(&db)
            .unwrap()
            .thread(thread_id)
            .unwrap()
            .unwrap()
    }
}

fn assistant_texts(snap: &ThreadSnapshot) -> Vec<String> {
    texts(snap, "assistant_message")
}

/// Wait until the agent's log has a frame passing `f` (the agent logs from
/// its own reader thread, after the core sent the frame).
async fn logged(path: &Path, f: impl Fn(&serde_json::Value) -> bool) -> Vec<serde_json::Value> {
    for _ in 0..200 {
        let frames: Vec<_> = read_log(path).into_iter().filter(|e| f(e)).collect();
        if !frames.is_empty() {
            return frames;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("nothing matching in {}", path.display());
}

fn read_log(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn claude_thread_end_to_end() {
    let dir = temp_dir("claude");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::ClaudeCode, false).await;
    assert_eq!(thread.provider, ProviderKind::ClaudeCode);
    assert_eq!(
        core.turn_approving(thread.id, "list files").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(thread.id).await;
    assert_eq!(kinds(&snap)[0], "user_message");
    assert!(assistant_texts(&snap).concat().contains("Done."));
    let run = snap.runs.last().unwrap();
    assert!(
        run.provider_turn_id
            .as_deref()
            .unwrap()
            .starts_with("fake-claude-session-t1")
    );
    assert_eq!(run.provider, ProviderKind::ClaudeCode);
    // Same process, second turn.
    assert_eq!(
        core.turn(thread.id, "echo: again").await,
        RunStatus::Completed
    );
    let state = core.thread_state(thread.id).await;
    assert_eq!(
        state.provider_thread_id.as_deref(),
        Some("fake-claude-session")
    );
    core.shutdown();
}

#[tokio::test]
async fn models_are_reported_per_provider() {
    let dir = temp_dir("models");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Antigravity, false).await;
    core.send(thread.id, "echo: hi");
    let (provider, models) = core
        .until(|e| match e {
            CoreEvent::Models { provider, models } => Some((*provider, models.clone())),
            _ => None,
        })
        .await;
    assert_eq!(provider, ProviderKind::Antigravity);
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["fake-model-a", "fake-model-b"]
    );
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    core.shutdown();
}

#[tokio::test]
async fn antigravity_thread_end_to_end_with_model() {
    let dir = temp_dir("agy");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Antigravity, false).await;
    assert_eq!(
        core.turn_approving(thread.id, "list files").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(thread.id).await;
    assert!(snap.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::ToolCall {
            status: ToolStatus::Completed,
            ..
        } | ItemKind::CommandExecution {
            status: ToolStatus::Completed,
            ..
        }
    )));
    // Switch model (same provider): the session restarts, resumes with
    // session/load and selects the model.
    let c = core.dispatch(Command::ThreadSetProvider {
        thread_id: thread.id,
        provider: ProviderKind::Antigravity,
        model: Some("fake-model-b".into()),
    });
    core.accepted(&c).await;
    assert_eq!(core.turn(thread.id, "model?").await, RunStatus::Completed);
    let snap = core.snapshot(thread.id).await;
    assert_eq!(assistant_texts(&snap).last().unwrap(), "model=fake-model-b");
    let state = core.thread_state(thread.id).await;
    assert_eq!(state.provider_thread_id.as_deref(), Some("sess-fake-1"));
    assert_eq!(state.model.as_deref(), Some("fake-model-b"));
    core.shutdown();
}

#[tokio::test]
async fn queued_messages_start_in_order_after_the_active_run() {
    let dir = temp_dir("queue");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    core.send(thread.id, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    core.send(thread.id, "echo: second");
    core.send(thread.id, "echo: third");
    let mut finished = Vec::new();
    while finished.len() < 3 {
        finished.push(core.run_finished().await);
    }
    assert_eq!(finished, vec![RunStatus::Completed; 3]);
    let snap = core.snapshot(thread.id).await;
    // Each queued message moved to where its turn started.
    let order: Vec<String> = snap
        .items
        .iter()
        .filter(|i| i.kind.is_text())
        .map(|i| i.text.to_string())
        .map(|t| t.chars().take(12).collect())
        .collect();
    assert_eq!(order[0], "slow");
    assert!(order[1].starts_with("tick 1"));
    assert_eq!(order[2], "echo: second");
    assert_eq!(order[3], "echo: second");
    assert_eq!(order[4], "echo: third");
    assert!(snap.runs.iter().all(|r| r.status == RunStatus::Completed));
    core.shutdown();
}

async fn steer_once(provider: ProviderKind) {
    let dir = temp_dir(&format!("steer-{}", provider.id()));
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, provider, false).await;
    core.send(thread.id, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let steer = core.send_with(thread.id, "echo: change of plan", Delivery::Steer);
    core.accepted(&steer).await;
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let snap = core.snapshot(thread.id).await;
    // One run, both messages in it.
    assert_eq!(snap.runs.len(), 1, "{provider:?}");
    let users: Vec<_> = snap
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::UserMessage)
        .collect();
    assert_eq!(users.len(), 2);
    assert!(users.iter().all(|u| u.run_id == Some(snap.runs[0].id)));
    let all = assistant_texts(&snap).concat();
    assert!(all.contains("change of plan"), "{provider:?}: {all}");
    core.shutdown();
}

#[tokio::test]
async fn steer_codex() {
    steer_once(ProviderKind::Codex).await;
}

#[tokio::test]
async fn steer_claude() {
    steer_once(ProviderKind::ClaudeCode).await;
}

#[tokio::test]
async fn steer_antigravity_cancels_and_resends() {
    steer_once(ProviderKind::Antigravity).await;
}

/// The turn ends before the steer reaches it (Codex refuses the steer):
/// the message becomes a queued run of its own instead of a hidden turn.
#[tokio::test]
async fn steer_that_misses_the_turn_is_queued() {
    let dir = temp_dir("steer-missed");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    core.send(thread.id, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let steer = core.send_with(thread.id, "missed: echo: later", Delivery::Steer);
    core.accepted(&steer).await;
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    assert_eq!(core.run_finished().await, RunStatus::Completed);
    let snap = core.snapshot(thread.id).await;
    assert_eq!(snap.runs.len(), 2, "{:?}", snap.runs);
    let steered = snap
        .items
        .iter()
        .find(|i| i.kind == ItemKind::UserMessage && &*i.text == "missed: echo: later")
        .unwrap();
    // The message moved to the second run, which answered it.
    assert_eq!(steered.run_id, Some(snap.runs[1].id));
    let answers = assistant_texts(&snap);
    assert!(
        answers.last().unwrap().contains("missed: echo: later"),
        "{answers:?}"
    );
    assert!(snap.items.iter().any(|i| matches!(&i.kind,
        ItemKind::SystemNotice { message } if message.contains("queued as the next turn"))));
    core.shutdown();
}

#[tokio::test]
async fn fork_codex_natively_at_a_turn() {
    let dir = temp_dir("fork-codex");
    let log = dir.join("codex.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CODEX_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let source = core.thread(project, ProviderKind::Codex, false).await;
    assert_eq!(
        core.turn(source.id, "echo: one").await,
        RunStatus::Completed
    );
    assert_eq!(
        core.turn(source.id, "echo: two").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(source.id).await;
    let first_run = snap.runs[0].id;

    let fork_id = ThreadId::new();
    let c = core.dispatch(Command::ThreadFork {
        source_thread_id: source.id,
        thread_id: fork_id,
        up_to_run_id: Some(first_run),
    });
    let fork = core
        .until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } if thread.id == fork_id => Some(thread.clone()),
                _ => None,
            },
            CoreEvent::CommandRejected { reason, .. } => panic!("{reason}"),
            _ => None,
        })
        .await;
    let _ = c;
    assert_eq!(fork.forked_from, Some(source.id));
    assert!(fork.title.ends_with("(fork)"));
    assert_eq!(
        fork.pending_context,
        Some(PendingContext::Fork {
            provider_thread_id: "thread-fake-1".into(),
            up_to_turn: Some("turn-1".into()),
        })
    );
    let copied = core.snapshot(fork_id).await;
    assert_eq!(copied.runs.len(), 1);
    assert_eq!(texts(&copied, "user_message"), vec!["echo: one"]);

    assert_eq!(
        core.turn(fork_id, "echo: in the fork").await,
        RunStatus::Completed
    );
    let state = core.thread_state(fork_id).await;
    assert_eq!(state.provider_thread_id.as_deref(), Some("thread-fork-1"));
    assert_eq!(state.pending_context, None);
    let forks: Vec<_> = read_log(&log)
        .into_iter()
        .filter(|f| f["method"] == "thread/fork")
        .collect();
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["params"]["threadId"], "thread-fake-1");
    assert_eq!(forks[0]["params"]["lastTurnId"], "turn-1");
    // The source is untouched.
    let source_snap = core.snapshot(source.id).await;
    assert_eq!(source_snap.runs.len(), 2);
    core.shutdown();
}

#[tokio::test]
async fn fork_claude_uses_fork_session_flags() {
    let dir = temp_dir("fork-claude");
    let log = dir.join("claude.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CLAUDE_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let source = core.thread(project, ProviderKind::ClaudeCode, false).await;
    assert_eq!(
        core.turn(source.id, "echo: one").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(source.id).await;
    let turn = snap.runs[0].provider_turn_id.clone().unwrap();
    let fork_id = ThreadId::new();
    let c = core.dispatch(Command::ThreadFork {
        source_thread_id: source.id,
        thread_id: fork_id,
        up_to_run_id: None,
    });
    core.accepted(&c).await;
    assert_eq!(
        core.turn(fork_id, "echo: forked").await,
        RunStatus::Completed
    );
    let argv: Vec<Vec<String>> = read_log(&log)
        .into_iter()
        .filter_map(|e| serde_json::from_value(e["argv"].clone()).ok())
        .collect();
    let fork_argv = argv.last().unwrap();
    assert!(
        fork_argv
            .windows(2)
            .any(|w| w == ["--resume", "fake-claude-session"])
    );
    assert!(fork_argv.contains(&"--fork-session".to_owned()));
    assert!(fork_argv.contains(&format!("--resume-session-at={turn}")));
    let state = core.thread_state(fork_id).await;
    assert!(
        state
            .provider_thread_id
            .as_deref()
            .unwrap()
            .starts_with("fake-claude-session-fork-")
    );
    core.shutdown();
}

#[tokio::test]
async fn fork_antigravity_hands_the_context_over() {
    let dir = temp_dir("fork-agy");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let source = core.thread(project, ProviderKind::Antigravity, false).await;
    assert_eq!(
        core.turn(source.id, "echo: remember 42").await,
        RunStatus::Completed
    );
    let fork_id = ThreadId::new();
    let c = core.dispatch(Command::ThreadFork {
        source_thread_id: source.id,
        thread_id: fork_id,
        up_to_run_id: None,
    });
    core.accepted(&c).await;
    assert_eq!(
        core.thread_state(fork_id).await.pending_context,
        Some(PendingContext::Handoff)
    );
    assert_eq!(
        core.turn(fork_id, "echo: what was it?").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(fork_id).await;
    let reply = assistant_texts(&snap).last().unwrap().clone();
    assert!(reply.starts_with("<previous_conversation>"), "{reply}");
    assert!(reply.contains("User: echo: remember 42"), "{reply}");
    assert!(reply.ends_with("echo: what was it?"), "{reply}");
    // The handoff is consumed: the next turn is a plain prompt.
    assert_eq!(
        core.turn(fork_id, "echo: plain").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(fork_id).await;
    assert_eq!(assistant_texts(&snap).last().unwrap(), "echo: plain");
    core.shutdown();
}

#[tokio::test]
async fn provider_switch_hands_the_conversation_over() {
    let dir = temp_dir("switch");
    let log = dir.join("claude.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CLAUDE_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    assert_eq!(
        core.turn(thread.id, "echo: hello codex").await,
        RunStatus::Completed
    );
    let c = core.dispatch(Command::ThreadSetProvider {
        thread_id: thread.id,
        provider: ProviderKind::ClaudeCode,
        model: Some("fake-large".into()),
    });
    core.accepted(&c).await;
    let state = core.thread_state(thread.id).await;
    assert_eq!(state.provider, ProviderKind::ClaudeCode);
    assert_eq!(state.provider_thread_id, None);
    assert_eq!(state.pending_context, Some(PendingContext::Handoff));
    assert_eq!(
        core.turn(thread.id, "echo: and now?").await,
        RunStatus::Completed
    );
    let snap = core.snapshot(thread.id).await;
    let reply = assistant_texts(&snap).last().unwrap().clone();
    assert!(reply.contains("User: echo: hello codex"), "{reply}");
    assert!(reply.contains("Assistant: echo: hello codex"), "{reply}");
    assert!(snap.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::SystemNotice { message } if message.contains("Switched from Codex to Claude Code")
    )));
    let argv: Vec<String> = serde_json::from_value(read_log(&log)[0]["argv"].clone()).unwrap();
    assert!(!argv.contains(&"--resume".to_owned()));
    assert!(argv.windows(2).any(|w| w == ["--model", "fake-large"]));
    let state = core.thread_state(thread.id).await;
    assert_eq!(
        state.provider_thread_id.as_deref(),
        Some("fake-claude-session")
    );
    assert_eq!(state.pending_context, None);
    // Busy threads cannot switch.
    core.send(thread.id, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let c = core.dispatch(Command::ThreadSetProvider {
        thread_id: thread.id,
        provider: ProviderKind::Codex,
        model: None,
    });
    assert!(core.rejected(&c).await.contains("wait"));
    core.dispatch(Command::RunInterrupt {
        thread_id: thread.id,
    });
    core.run_finished().await;
    core.shutdown();
}

#[tokio::test]
async fn codex_rollback_restores_files_and_reverts_live() {
    let dir = temp_dir("rollback-codex");
    git_project(&dir);
    let log = dir.join("codex.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CODEX_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    let file = dir.join("project/notes.txt");
    assert_eq!(
        core.turn(thread.id, "write notes.txt first").await,
        RunStatus::Completed
    );
    assert_eq!(
        core.turn(thread.id, "write notes.txt second").await,
        RunStatus::Completed
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "second");
    let snap = core.snapshot(thread.id).await;
    assert!(snap.runs.iter().all(|r| r.checkpoint.is_some()));
    let second = snap.runs[1].clone();
    assert_eq!(second.provider_turn_id.as_deref(), Some("turn-2"));
    // The checkpoint lives under a hidden ref, not on a branch.
    let refs = git(
        &dir.join("project"),
        &["for-each-ref", "--format=%(refname)"],
    );
    assert!(refs.contains(&format!(
        "refs/blongo/checkpoints/{}/{}",
        thread.id, second.id
    )));
    assert_eq!(
        git(&dir.join("project"), &["status", "--porcelain"]),
        "?? notes.txt"
    );

    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: second.id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "first");
    let snap = core.snapshot(thread.id).await;
    assert_eq!(snap.runs[1].status, RunStatus::RolledBack);
    assert_eq!(texts(&snap, "user_message"), vec!["write notes.txt first"]);
    assert!(snap.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::SystemNotice { message } if message.contains("Rolled back 1 turn. Files were restored")
    )));
    // The live Codex thread dropped the turn natively.
    let reverts = logged(&log, |f| f["method"] == "thread/revert").await;
    assert_eq!(reverts.len(), 1);
    assert_eq!(reverts[0]["params"]["beforeTurnId"], "turn-2");

    // Roll back everything: the file did not exist before the first turn.
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: snap.runs[0].id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    assert!(!file.exists());
    assert_eq!(git(&dir.join("project"), &["status", "--porcelain"]), "");
    // The thread continues.
    assert_eq!(
        core.turn(thread.id, "echo: after").await,
        RunStatus::Completed
    );
    core.shutdown();
}

#[tokio::test]
async fn codex_rollback_after_restart_reverts_on_resume() {
    let dir = temp_dir("rollback-resume");
    let log = dir.join("codex.log");
    let tweak = |c: &mut CoreConfig| {
        c.agent_env
            .push(("FAKE_CODEX_LOG".into(), log.to_string_lossy().into_owned()))
    };
    let mut core = start_tweaked(&dir, tweak);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    core.turn(thread.id, "echo: one").await;
    core.turn(thread.id, "echo: two").await;
    let runs = core.snapshot(thread.id).await.runs.clone();
    core.shutdown();
    let mut core = start_tweaked(&dir, tweak);
    // No live session now: the rewind waits for the next session.
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: runs[1].id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    assert_eq!(
        core.thread_state(thread.id).await.pending_context,
        Some(PendingContext::Rewind {
            keep_through_turn: Some("turn-1".into()),
            drop_from_turn: Some("turn-2".into()),
        })
    );
    core.turn(thread.id, "echo: three").await;
    let frames = read_log(&log);
    let resume = frames
        .iter()
        .rposition(|f| f["method"] == "thread/resume")
        .unwrap();
    let revert = frames
        .iter()
        .rposition(|f| f["method"] == "thread/revert")
        .unwrap();
    assert!(resume < revert);
    assert_eq!(frames[revert]["params"]["beforeTurnId"], "turn-2");
    assert_eq!(core.thread_state(thread.id).await.pending_context, None);
    core.shutdown();
}

#[tokio::test]
async fn claude_rollback_resumes_at_the_kept_turn() {
    let dir = temp_dir("rollback-claude");
    let log = dir.join("claude.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CLAUDE_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::ClaudeCode, false).await;
    core.turn(thread.id, "echo: one").await;
    core.turn(thread.id, "echo: two").await;
    let runs = core.snapshot(thread.id).await.runs.clone();
    let kept = runs[0].provider_turn_id.clone().unwrap();
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: runs[1].id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    core.turn(thread.id, "echo: three").await;
    let argv: Vec<String> = serde_json::from_value(
        read_log(&log)
            .iter()
            .rev()
            .find(|e| e.get("argv").is_some())
            .unwrap()["argv"]
            .clone(),
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|w| w == ["--resume", "fake-claude-session"])
    );
    assert!(argv.contains(&format!("--resume-session-at={kept}")));
    assert!(!argv.contains(&"--fork-session".to_owned()));
    core.shutdown();
}

#[tokio::test]
async fn claude_rollback_past_a_turn_without_an_id_keeps_the_newest_named_one() {
    let dir = temp_dir("rollback-claude-noid");
    let log = dir.join("claude.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CLAUDE_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::ClaudeCode, false).await;
    core.turn(thread.id, "echo: one").await;
    // Interrupted before the agent named the turn.
    core.send(thread.id, "loop");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    core.dispatch(Command::RunInterrupt {
        thread_id: thread.id,
    });
    core.run_finished().await;
    core.turn(thread.id, "echo: three").await;
    let runs = core.snapshot(thread.id).await.runs.clone();
    assert!(runs[1].provider_turn_id.is_none());
    let kept = runs[0].provider_turn_id.clone().unwrap();
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: runs[2].id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    assert_eq!(
        core.thread_state(thread.id).await.pending_context,
        Some(PendingContext::Rewind {
            keep_through_turn: Some(kept),
            drop_from_turn: runs[2].provider_turn_id.clone(),
        })
    );
    core.shutdown();
}

#[tokio::test]
async fn claude_fork_after_rollback_is_still_native() {
    let dir = temp_dir("rollback-fork-claude");
    let log = dir.join("claude.log");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env
            .push(("FAKE_CLAUDE_LOG".into(), log.to_string_lossy().into_owned()))
    });
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::ClaudeCode, false).await;
    core.turn(thread.id, "echo: one").await;
    core.turn(thread.id, "echo: two").await;
    let runs = core.snapshot(thread.id).await.runs.clone();
    let kept = runs[0].provider_turn_id.clone().unwrap();
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: runs[1].id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    let fork_id = ThreadId::new();
    let c = core.dispatch(Command::ThreadFork {
        source_thread_id: thread.id,
        thread_id: fork_id,
        up_to_run_id: None,
    });
    core.accepted(&c).await;
    core.turn(fork_id, "echo: forked").await;
    let argv: Vec<String> = serde_json::from_value(
        read_log(&log)
            .iter()
            .rev()
            .find(|e| e.get("argv").is_some())
            .unwrap()["argv"]
            .clone(),
    )
    .unwrap();
    // Branches at the kept turn: the rolled back one is not in the fork.
    assert!(argv.contains(&"--fork-session".to_owned()));
    assert!(argv.contains(&format!("--resume-session-at={kept}")));
    core.shutdown();
}

#[tokio::test]
async fn rollback_is_refused_while_running() {
    let dir = temp_dir("rollback-busy");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    core.turn(thread.id, "echo: one").await;
    let run = core.snapshot(thread.id).await.runs[0].id;
    core.send(thread.id, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: run,
        acknowledged_sharers: 0,
    });
    assert!(core.rejected(&c).await.contains("wait"));
    core.dispatch(Command::RunInterrupt {
        thread_id: thread.id,
    });
    core.run_finished().await;
    core.shutdown();
}

#[tokio::test]
async fn rollback_keeps_the_replaced_files() {
    let dir = temp_dir("rollback-pre");
    git_project(&dir);
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    let repo = dir.join("project");
    core.turn(thread.id, "write notes.txt first").await;
    core.turn(thread.id, "write notes.txt second").await;
    let runs = core.snapshot(thread.id).await.runs.clone();
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: runs[1].id,
        acknowledged_sharers: 0,
    });
    core.accepted(&c).await;
    assert_eq!(
        std::fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "first"
    );
    let pre = format!("refs/blongo/pre-rollback/{}/{}", thread.id, runs[1].id);
    let snap = core.snapshot(thread.id).await;
    assert!(snap.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::SystemNotice { message } if message.contains(&pre)
    )));
    // The saved tree has what the rollback replaced.
    assert_eq!(git(&repo, &["show", &format!("{pre}:notes.txt")]), "second");
    git(
        &repo,
        &[
            "restore",
            &format!("--source={pre}"),
            "--worktree",
            "--",
            ".",
        ],
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "second"
    );
    // Archiving the thread drops its hidden refs.
    let c = core.dispatch(Command::ThreadArchive {
        thread_id: thread.id,
    });
    core.accepted(&c).await;
    core.snapshot(thread.id).await;
    assert_eq!(
        git(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/blongo/"]
        ),
        ""
    );
    core.shutdown();
}

#[tokio::test]
async fn rollback_of_a_shared_folder_needs_every_thread_idle_and_a_confirmation() {
    let dir = temp_dir("rollback-shared");
    git_project(&dir);
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    let other = core.thread(project, ProviderKind::Codex, false).await;
    // A worktree thread works elsewhere and does not count.
    let elsewhere = core.thread(project, ProviderKind::Codex, true).await;
    core.turn(thread.id, "write notes.txt first").await;
    let run = core.snapshot(thread.id).await.runs[0].id;

    // The other thread is running: refused whatever the confirmation says.
    core.send(other.id, "slow");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: run,
        acknowledged_sharers: 1,
    });
    assert!(
        core.rejected(&c)
            .await
            .contains("same folder and is running")
    );
    core.dispatch(Command::RunInterrupt {
        thread_id: other.id,
    });
    core.run_finished().await;

    // Idle now, but the user was not told about it.
    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: run,
        acknowledged_sharers: 0,
    });
    assert!(
        core.rejected(&c)
            .await
            .contains("1 other thread works in this folder")
    );
    assert!(dir.join("project/notes.txt").exists());

    let c = core.dispatch(Command::ThreadRollback {
        thread_id: thread.id,
        run_id: run,
        acknowledged_sharers: 1,
    });
    core.accepted(&c).await;
    assert!(!dir.join("project/notes.txt").exists());
    let snap = core.snapshot(thread.id).await;
    assert!(snap.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::SystemNotice { message } if message.contains("1 other thread in this folder")
    )));
    // Archived with nothing uncommitted, its worktree goes (the branch stays).
    let c = core.dispatch(Command::ThreadArchive {
        thread_id: elsewhere.id,
    });
    core.accepted(&c).await;
    core.snapshot(thread.id).await;
    assert!(!Path::new(&elsewhere.worktree.as_ref().unwrap().path).exists());
    core.shutdown();
}

#[tokio::test]
async fn worktree_threads_work_on_their_own_branch() {
    let dir = temp_dir("worktree");
    git_project(&dir);
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, true).await;
    let worktree = thread.worktree.clone().unwrap();
    assert!(worktree.branch.starts_with("blongo/"));
    let path = PathBuf::from(&worktree.path);
    assert!(path.starts_with(dir.join("data/worktrees")));
    assert_eq!(
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        worktree.branch
    );
    assert_eq!(
        core.turn(thread.id, "write only-here.txt x").await,
        RunStatus::Completed
    );
    assert!(path.join("only-here.txt").exists());
    assert!(!dir.join("project/only-here.txt").exists());
    // Not a repository: refused, nothing created.
    let plain = temp_dir("worktree-plain");
    let other = core.project(&plain).await;
    let c = core.dispatch(Command::ThreadCreate {
        thread_id: ThreadId::new(),
        project_id: other,
        title: String::new(),
        provider: ProviderKind::Codex,
        model: None,
        worktree: true,
    });
    assert!(core.rejected(&c).await.contains("not in a git repository"));
    core.shutdown();
}

#[tokio::test]
async fn plans_are_one_item_updated_in_place() {
    for provider in [
        ProviderKind::Codex,
        ProviderKind::ClaudeCode,
        ProviderKind::Antigravity,
    ] {
        let dir = temp_dir(&format!("plan-{}", provider.id()));
        let mut core = start(&dir);
        let project = core.project(&dir).await;
        let thread = core.thread(project, provider, false).await;
        assert_eq!(core.turn(thread.id, "plan").await, RunStatus::Completed);
        let snap = core.snapshot(thread.id).await;
        let plans: Vec<_> = snap
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                ItemKind::Plan { steps } => Some(steps.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(plans.len(), 1, "{provider:?}");
        assert_eq!(plans[0].len(), 2);
        assert!(
            plans[0].iter().all(|s| s.status == PlanStatus::Completed),
            "{provider:?}"
        );
        assert_eq!(plans[0][0].text, "Read the code");
        // No TodoWrite tool row for Claude.
        assert!(!snap.items.iter().any(|i| matches!(
            &i.kind,
            ItemKind::ToolCall { name, .. } if name == "TodoWrite"
        )));
        core.shutdown();
    }
}

#[tokio::test]
async fn antigravity_sign_in_through_the_core() {
    let dir = temp_dir("login");
    let done = dir.join("signed-in");
    let mut core = start_tweaked(&dir, |c| {
        c.agent_env.push(("FAKE_ACP_SIGNED_OUT".into(), "1".into()));
        c.agent_env.push((
            "FAKE_ACP_LOGIN_FILE".into(),
            done.to_string_lossy().into_owned(),
        ));
    });
    core.handle().client().login(ProviderKind::Antigravity);
    let url = core
        .until(|e| match e {
            CoreEvent::Login {
                state: blongo_core::LoginState::Url(url),
                ..
            } => Some(url.clone()),
            _ => None,
        })
        .await;
    assert!(url.starts_with("https://accounts.google.com/"));
    std::fs::write(&done, "").unwrap();
    let state = core
        .until(|e| match e {
            CoreEvent::Login { state, .. } if !matches!(state, blongo_core::LoginState::Url(_)) => {
                Some(state.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(state, blongo_core::LoginState::Succeeded);
    // CLI providers are signed in with their own CLI.
    core.handle().client().login(ProviderKind::ClaudeCode);
    let state = core
        .until(|e| match e {
            CoreEvent::Login {
                provider: ProviderKind::ClaudeCode,
                state,
            } => Some(state.clone()),
            _ => None,
        })
        .await;
    assert!(matches!(state, blongo_core::LoginState::Failed(m) if m.contains("/login")));
    core.shutdown();
}

#[tokio::test]
async fn install_refuses_an_unpinned_origin() {
    let dir = temp_dir("install");
    let root = dir.join("agy");
    let mut core = start_tweaked(&dir, |c| {
        c.antigravity_install.root = Some(root.clone());
        c.antigravity_install.pin = Some(blongo_harness::antigravity_install::ArchivePin {
            version: "0.0.1".into(),
            url: "https://example.invalid/agy.zip".into(),
            entry: "agy_acp_server.par".into(),
            sha512: "00".repeat(64),
        });
    });
    core.handle().client().install_antigravity();
    let state = core
        .until(|e| match e {
            CoreEvent::Install(blongo_core::InstallState::Failed(m)) => Some(m.clone()),
            CoreEvent::Install(blongo_core::InstallState::Done(_)) => panic!("installed"),
            _ => None,
        })
        .await;
    assert!(state.contains("refusing"), "{state}");
    core.shutdown();
}

#[tokio::test]
async fn queued_runs_left_by_a_crash_are_not_sent() {
    let dir = temp_dir("queue-crash");
    let mut core = start(&dir);
    let project = core.project(&dir).await;
    let thread = core.thread(project, ProviderKind::Codex, false).await;
    core.send(thread.id, "loop");
    core.until(|e| matches!(e, CoreEvent::TextDelta { .. }).then_some(()))
        .await;
    let q = core.send(thread.id, "echo: later");
    core.accepted(&q).await;
    core.abort();
    let mut core = start(&dir);
    let snap = core.snapshot(thread.id).await;
    assert!(snap.runs.iter().all(|r| r.status == RunStatus::Interrupted));
    assert!(snap.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::SystemNotice { message } if message.starts_with("Not sent")
    )));
    core.shutdown();
}
