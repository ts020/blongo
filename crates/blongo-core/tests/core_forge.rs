//! GitHub pull request links against a fake GitHub (tools/fixtures/
//! fake_github.py): linking by reference, finding a pushed branch's pull
//! request, polling and refreshing its status, unlinking, persistence, and
//! no API calls for threads that never pushed.
#![cfg(unix)]

mod common;

use blongo_forge::forge::{ForgeKind, TokenFile};
use blongo_forge::http::{self, Request};
use blongo_protocol::workspace::{Query, QueryReply};
use blongo_protocol::{ChecksState, PrLink, PrState, PrStatus, Thread};
use common::*;
use serde_json::{Value, json};
use std::collections::HashMap;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `<dir>/project`: a repository whose `origin` is
/// `https://github.com/acme/widgets.git`, rewritten (for fetch and push)
/// to a bare repository in `<dir>/remote.git`.
fn github_project(dir: &Path) {
    let bare = dir.join("remote.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
    let project = dir.join("project");
    git(&project, &["init", "--quiet", "-b", "main"]);
    git(&project, &["config", "user.name", "Test"]);
    git(&project, &["config", "user.email", "test@localhost"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    git(
        &project,
        &[
            "config",
            &format!("url.{}.insteadOf", bare.display()),
            "https://github.com/acme/widgets.git",
        ],
    );
    git(
        &project,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    std::fs::write(project.join("README.md"), "readme\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "init"]);
    git(&project, &["push", "--quiet", "-u", "origin", "main"]);
}

struct FakeGitHub {
    child: std::process::Child,
    api: String,
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl FakeGitHub {
    fn start(dir: &Path) -> Self {
        let port_file = dir.join("github.port");
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/fixtures/fake_github.py");
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .arg(&port_file)
            .spawn()
            .expect("python3 for the fake GitHub");
        // A cold python3 on a busy macOS runner can take well over 10 s.
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let port = loop {
            if let Ok(p) = std::fs::read_to_string(&port_file)
                && !p.is_empty()
            {
                break p;
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!("fake GitHub exited: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fake GitHub did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        Self {
            child,
            api: format!("http://127.0.0.1:{}", port.trim()),
        }
    }

    async fn control(&self, body: Value) {
        let resp = http::send(Request::post_json(format!("{}/__control", self.api), &body))
            .await
            .unwrap();
        assert!(resp.ok(), "control {body}: {}", resp.text());
    }

    async fn pull(&self, fields: Value) {
        self.control(json!({"op": "pull", "repo": "acme/widgets", "pull": fields}))
            .await;
    }

    async fn log(&self) -> Vec<Value> {
        let resp = http::send(Request::get(format!("{}/__log", self.api)))
            .await
            .unwrap();
        serde_json::from_slice(&resp.body).unwrap()
    }
}

/// A core talking to the fake GitHub with a saved token.
async fn start(dir: &Path) -> (TestCore, FakeGitHub) {
    let gh = FakeGitHub::start(dir);
    gh.control(json!({"op": "repo", "repo": "acme/widgets", "default_branch": "main"}))
        .await;
    let tokens = dir.join("forge.json");
    let mut file = TokenFile::default();
    file.set(ForgeKind::GitHub, None, "test-token".into());
    file.save(&tokens).unwrap();
    let api = gh.api.clone();
    let (core, _) = TestCore::start_with(dir, |c| {
        c.forge_tokens = tokens;
        c.github_api = Some(api);
    });
    (core, gh)
}

impl TestCore {
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

    async fn project(&mut self, dir: &Path) -> ProjectId {
        let project_id = ProjectId::new();
        let c = self.dispatch(Command::ProjectCreate {
            project_id,
            name: String::new(),
            path: dir.join("project").to_string_lossy().into_owned(),
        });
        self.ok(&c).await;
        project_id
    }

    async fn thread(&mut self, project_id: ProjectId, worktree: bool) -> Thread {
        let thread_id = ThreadId::new();
        self.dispatch(Command::ThreadCreate {
            thread_id,
            project_id,
            title: String::new(),
            provider: ProviderKind::Codex,
            model: None,
            worktree,
            parent_thread_id: None,
        });
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } if thread.id == thread_id => {
                    Some((**thread).clone())
                }
                _ => None,
            },
            _ => None,
        })
        .await
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

    fn send_query(&mut self, query: Query) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 40);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.handle().client().query(id, query);
        id
    }

    /// The replies to `ids`, in that order, whatever order they come in.
    async fn replies(&mut self, ids: &[u64]) -> Vec<Result<QueryReply, String>> {
        let mut got: HashMap<u64, Result<QueryReply, String>> = HashMap::new();
        while got.len() < ids.len() {
            let (id, result) = self
                .until(|e| match e {
                    CoreEvent::Reply { id, result } if ids.contains(id) => {
                        Some((*id, result.clone()))
                    }
                    _ => None,
                })
                .await;
            got.insert(id, result);
        }
        ids.iter().map(|id| got.remove(id).unwrap()).collect()
    }

    /// `PrRefresh`, with the status events that came before its reply.
    async fn refresh(
        &mut self,
        thread_id: ThreadId,
    ) -> (Result<QueryReply, String>, Option<PrStatus>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.handle()
            .client()
            .query(id, Query::PrRefresh { thread_id });
        let mut last = None;
        let reply = self
            .until(|e| match e {
                CoreEvent::Reply { id: got, result } if *got == id => Some(result.clone()),
                CoreEvent::Event(ev) => {
                    if let EventKind::ThreadPrStatus {
                        thread_id: t,
                        status: Some(s),
                    } = &ev.kind
                        && *t == thread_id
                    {
                        last = Some(s.clone());
                    }
                    None
                }
                _ => None,
            })
            .await;
        (reply, last)
    }

    async fn linked(&mut self, thread_id: ThreadId) -> Option<PrLink> {
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadPrLinked {
                    thread_id: t, pr, ..
                } if *t == thread_id => Some(pr.clone()),
                _ => None,
            },
            _ => None,
        })
        .await
    }

    async fn status(&mut self, thread_id: ThreadId) -> PrStatus {
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadPrStatus {
                    thread_id: t,
                    status: Some(s),
                } if *t == thread_id => Some(s.clone()),
                _ => None,
            },
            _ => None,
        })
        .await
    }
}

#[tokio::test]
async fn link_poll_refresh_unlink() {
    let dir = temp_dir("forge-link");
    github_project(&dir);
    let (mut core, gh) = start(&dir).await;
    gh.pull(json!({
        "number": 7,
        "title": "Speed up the parser",
        "head": "fast-parser",
        "checks": [{"name": "test", "status": "IN_PROGRESS", "conclusion": null}],
    }))
    .await;
    let project = core.project(&dir).await;
    let thread = core.thread(project, false).await;

    // Bad references are refused before anything is asked.
    let c = core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "not a pr".into(),
    });
    assert!(core.rejected(&c).await.contains("pull request URL"));
    let c = core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "#99".into(),
    });
    assert!(
        core.rejected(&c)
            .await
            .contains("acme/widgets#99 was not found")
    );

    core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "#7".into(),
    });
    let link = core.linked(thread.id).await.unwrap();
    assert_eq!(link.repo, "acme/widgets");
    assert_eq!(link.host, "github.com");
    assert_eq!(link.number, 7);
    assert_eq!(link.head_branch, "fast-parser");
    assert_eq!(link.base_branch, "main");
    assert!(!link.read_only);
    assert_eq!(link.url, "https://github.com/acme/widgets/pull/7");
    let status = core.status(thread.id).await;
    assert_eq!(status.title, "Speed up the parser");
    assert_eq!(status.checks.state, ChecksState::Pending);

    // The token went to the fake only, in a header.
    let log = gh.log().await;
    assert!(log.iter().all(|r| r["auth"] == "Bearer test-token"));
    assert!(log.iter().any(|r| r["path"] == "/graphql"));

    // CI fails; a refresh sees it.
    gh.pull(json!({"number": 7, "checks": [
        {"name": "test", "status": "COMPLETED", "conclusion": "FAILURE"}
    ]}))
    .await;
    let (reply, status) = core.refresh(thread.id).await;
    assert!(matches!(reply, Ok(QueryReply::Done(_))), "{reply:?}");
    let status = status.unwrap();
    assert_eq!(status.checks.state, ChecksState::Failure);
    assert_eq!(status.checks.failed, 1);

    // An outage is reported on the status and keeps what was seen.
    gh.control(json!({"op": "fail", "path_prefix": "/graphql", "status": 502}))
        .await;
    let (reply, status) = core.refresh(thread.id).await;
    assert!(reply.is_err(), "{reply:?}");
    let status = status.unwrap();
    assert!(
        status.error.as_deref().unwrap().contains("502"),
        "{status:?}"
    );
    assert_eq!(status.checks.state, ChecksState::Failure);

    // Merged: polled once more, then never again.
    gh.pull(json!({"number": 7, "merged": true, "state": "closed"}))
        .await;
    let (reply, status) = core.refresh(thread.id).await;
    reply.unwrap();
    let status = status.unwrap();
    assert_eq!(status.state, PrState::Merged);
    assert_eq!(status.error, None);

    // Unlinking is remembered.
    core.dispatch(Command::ThreadUnlinkPr {
        thread_id: thread.id,
    });
    assert_eq!(core.linked(thread.id).await, None);
    core.shutdown();

    let store = blongo_core::Store::open(&dir.join("data/blongo.sqlite")).unwrap();
    let t = store.thread(thread.id).unwrap().unwrap();
    assert!(t.pr.is_none());
    assert!(t.pr_dismissed);
}

#[tokio::test]
async fn finds_the_pushed_branch_pull_request() {
    let dir = temp_dir("forge-find");
    github_project(&dir);
    let (mut core, gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.thread(project, true).await;
    let branch = thread.worktree.as_ref().unwrap().branch.clone();
    let worktree = PathBuf::from(&thread.worktree.as_ref().unwrap().path);

    // Not pushed: no API call at all.
    gh.control(json!({"op": "clear_log"})).await;
    let reply = core
        .query(Query::PrRefresh {
            thread_id: thread.id,
        })
        .await;
    assert_eq!(
        reply,
        Ok(QueryReply::Done("no pull request for this branch".into()))
    );
    assert!(gh.log().await.is_empty(), "{:?}", gh.log().await);

    // Pushed, but no pull request yet.
    git(&worktree, &["push", "--quiet", "origin", &branch]);
    let reply = core
        .query(Query::PrRefresh {
            thread_id: thread.id,
        })
        .await;
    assert_eq!(
        reply,
        Ok(QueryReply::Done("no pull request for this branch".into()))
    );

    gh.pull(
        json!({"number": 12, "title": "Worktree work", "head": branch, "review": "APPROVED",
        "checks": [{"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}]}),
    )
    .await;
    let (reply, status) = core.refresh(thread.id).await;
    assert_eq!(reply, Ok(QueryReply::Done("linked acme/widgets#12".into())));
    let status = status.unwrap();
    assert_eq!(status.badge(), blongo_protocol::PrBadge::ReadyToMerge);
    core.shutdown();

    // The link survives a restart.
    let (core, shell) = start_again(&dir, &gh);
    let t = shell.threads.iter().find(|t| t.id == thread.id).unwrap();
    assert_eq!(t.pr.as_ref().map(|p| p.number), Some(12));
    assert_eq!(
        t.pr_status.as_ref().map(|s| s.badge()),
        Some(blongo_protocol::PrBadge::ReadyToMerge)
    );
    core.shutdown();
}

fn start_again(dir: &Path, gh: &FakeGitHub) -> (TestCore, Arc<ShellSnapshot>) {
    let api = gh.api.clone();
    let tokens = dir.join("forge.json");
    TestCore::start_with(dir, |c| {
        c.forge_tokens = tokens;
        c.github_api = Some(api);
    })
}

#[tokio::test]
async fn read_only_and_missing_token() {
    let dir = temp_dir("forge-ro");
    github_project(&dir);
    let gh = FakeGitHub::start(&dir);
    gh.control(json!({"op": "repo", "repo": "acme/widgets", "push": false}))
        .await;
    gh.pull(json!({"number": 3, "head": "fork-branch", "head_repo": "someone/widgets"}))
        .await;
    // No saved token, and a `gh` that has none either.
    let api = gh.api.clone();
    let tokens = dir.join("forge.json");
    let fake_gh = dir.join("gh");
    std::fs::write(&fake_gh, "#!/bin/sh\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (mut core, _) = TestCore::start_with(&dir, |c| {
        c.forge_tokens = tokens.clone();
        c.github_api = Some(api);
        c.gh_program = Some(fake_gh);
    });
    let project = core.project(&dir).await;
    let thread = core.thread(project, false).await;
    let c = core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "https://github.com/acme/widgets/pull/3".into(),
    });
    assert!(core.rejected(&c).await.contains("no GitHub token"));

    let mut file = TokenFile::default();
    file.set(ForgeKind::GitHub, None, "test-token".into());
    file.save(&tokens).unwrap();
    core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "acme/widgets#3".into(),
    });
    let link = core.linked(thread.id).await.unwrap();
    assert!(link.read_only, "a fork's pull request without push access");
    core.shutdown();
}

#[tokio::test]
async fn threads_sharing_a_pull_request_and_unlinked_refreshes() {
    let dir = temp_dir("forge-shared");
    github_project(&dir);
    let (mut core, gh) = start(&dir).await;
    gh.pull(json!({"number": 7, "head": "fast-parser"})).await;
    let project = core.project(&dir).await;
    let a = core.thread(project, false).await;
    let b = core.thread(project, false).await;
    for t in [a.id, b.id] {
        core.dispatch(Command::ThreadLinkPr {
            thread_id: t,
            pr: "#7".into(),
        });
        core.linked(t).await.unwrap();
    }

    // Both refreshes land in one poll; each thread gets the status.
    gh.pull(json!({"number": 7, "checks": [
        {"name": "test", "status": "COMPLETED", "conclusion": "FAILURE"}
    ]}))
    .await;
    let qa = core.send_query(Query::PrRefresh { thread_id: a.id });
    let qb = core.send_query(Query::PrRefresh { thread_id: b.id });
    for reply in core.replies(&[qa, qb]).await {
        assert_eq!(
            reply,
            Ok(QueryReply::Done("acme/widgets#7 checked".into())),
            "{reply:?}"
        );
    }

    // A refresh still pending when the link goes is answered.
    let q = core.send_query(Query::PrRefresh { thread_id: a.id });
    core.dispatch(Command::ThreadUnlinkPr { thread_id: a.id });
    let reply = tokio::time::timeout(Duration::from_secs(10), core.replies(&[q]))
        .await
        .expect("the refresh was answered")
        .remove(0);
    assert!(matches!(reply, Ok(QueryReply::Done(_))), "{reply:?}");
    core.shutdown();
}

async fn detail(core: &mut TestCore, thread_id: ThreadId) -> blongo_protocol::PrDetail {
    match core.query(Query::PrDetail { thread_id }).await {
        Ok(QueryReply::PrDetail(d)) => *d,
        other => panic!("detail: {other:?}"),
    }
}

#[tokio::test]
async fn pr_tab_detail_and_edit() {
    let dir = temp_dir("forge-detail");
    github_project(&dir);
    let (mut core, gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.thread(project, true).await;
    let branch = thread.worktree.as_ref().unwrap().branch.clone();
    let worktree = PathBuf::from(&thread.worktree.as_ref().unwrap().path);
    git(&worktree, &["push", "--quiet", "-u", "origin", &branch]);
    gh.pull(json!({
        "number": 9, "title": "Tab", "head": branch, "body": "Original body",
        "merge_state": "BLOCKED", "review": "CHANGES_REQUESTED",
        "reviews": [{"author": "alice", "state": "CHANGES_REQUESTED"}],
        "checks": [
            {"name": "test", "status": "COMPLETED", "conclusion": "FAILURE", "workflow": "CI",
             "url": "https://github.com/acme/widgets/actions/runs/1/job/2",
             "started_at": "2026-10-04T00:00:00Z", "completed_at": "2026-10-04T00:02:00Z"},
            {"name": "lint", "status": "IN_PROGRESS", "conclusion": null}
        ],
        "threads": [
            {"path": "src/lib.rs", "line": 3, "comments": [{"author": "alice", "body": "Rename this"}]},
            true
        ],
    }))
    .await;
    core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "#9".into(),
    });
    core.linked(thread.id).await.unwrap();

    let d = detail(&mut core, thread.id).await;
    assert_eq!(d.link.as_ref().map(|l| l.number), Some(9));
    assert_eq!(d.body, "Original body");
    assert_eq!(d.author, "octocat");
    assert_eq!(d.merge_state, blongo_protocol::MergeState::Blocked);
    assert!(d.can_edit);
    assert_eq!(d.checks.len(), 2);
    assert_eq!(d.checks[0].state, blongo_protocol::CheckState::Failure);
    assert_eq!(d.checks[0].workflow.as_deref(), Some("CI"));
    assert_eq!(d.checks[0].duration_secs, Some(120));
    assert_eq!(d.checks[1].state, blongo_protocol::CheckState::Pending);
    assert_eq!(d.reviews[0].author, "alice");
    assert_eq!(d.threads.len(), 2);
    assert_eq!(d.threads[0].comments[0].body, "Rename this");
    assert!(d.threads[1].resolved);
    assert_eq!((d.ahead, d.behind, d.uncommitted), (Some(0), Some(0), 0));
    assert_eq!(
        d.blockers(),
        [
            "1 check failing",
            "1 check running",
            "changes requested",
            "1 unresolved conversation"
        ]
    );

    // A local commit and an uncommitted file show up.
    std::fs::write(worktree.join("a.txt"), "a\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(
        &worktree,
        &["-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "a"],
    );
    std::fs::write(worktree.join("b.txt"), "b\n").unwrap();
    let d = detail(&mut core, thread.id).await;
    assert_eq!((d.ahead, d.uncommitted), (Some(1), 1));
    assert!(
        d.blockers()
            .contains(&"local commits not pushed".to_owned())
    );

    // Edits go to GitHub; bad ones are refused before.
    let bad = core
        .query(Query::PrEdit {
            thread_id: thread.id,
            title: Some("  ".into()),
            body: None,
        })
        .await;
    assert!(bad.unwrap_err().contains("title"));
    let reply = core
        .query(Query::PrEdit {
            thread_id: thread.id,
            title: Some("Better title\n".into()),
            body: Some("New body".into()),
        })
        .await;
    assert_eq!(reply, Ok(QueryReply::Done("acme/widgets#9 updated".into())));
    let patch = gh
        .log()
        .await
        .into_iter()
        .find(|r| r["method"] == "PATCH")
        .unwrap();
    assert_eq!(
        patch["body"],
        json!({"title": "Better title", "body": "New body"})
    );
    let d = detail(&mut core, thread.id).await;
    assert_eq!(
        (d.status.title.as_str(), d.body.as_str()),
        ("Better title", "New body")
    );

    // Nothing linked: refused.
    let other = core.thread(project, false).await;
    let none = core
        .query(Query::PrDetail {
            thread_id: other.id,
        })
        .await;
    assert!(none.unwrap_err().contains("no pull request"));
    core.shutdown();
}

#[tokio::test]
async fn read_only_links_cannot_edit() {
    let dir = temp_dir("forge-ro-edit");
    github_project(&dir);
    let (mut core, gh) = start(&dir).await;
    gh.control(json!({"op": "repo", "repo": "acme/widgets", "push": false}))
        .await;
    gh.pull(json!({"number": 4})).await;
    let project = core.project(&dir).await;
    let thread = core.thread(project, false).await;
    core.dispatch(Command::ThreadLinkPr {
        thread_id: thread.id,
        pr: "#4".into(),
    });
    assert!(core.linked(thread.id).await.unwrap().read_only);
    let d = detail(&mut core, thread.id).await;
    assert!(!d.can_edit);
    let reply = core
        .query(Query::PrEdit {
            thread_id: thread.id,
            title: Some("x".into()),
            body: None,
        })
        .await;
    assert!(reply.unwrap_err().contains("read-only"));
    assert!(gh.log().await.iter().all(|r| r["method"] != "PATCH"));
    core.shutdown();
}

#[tokio::test]
async fn forge_settings_are_validated_and_kept() {
    let dir = temp_dir("forge-settings");
    let (mut core, _) = TestCore::start(&dir);
    let project = core.project(&dir).await;
    let mut settings = blongo_protocol::ForgeSettings {
        branch_prefix: "-oops".into(),
        ..Default::default()
    };
    let c = core.dispatch(Command::ProjectSetForge {
        project_id: project,
        settings: settings.clone(),
    });
    assert!(
        core.rejected(&c)
            .await
            .contains("cannot start a branch name")
    );
    settings.branch_prefix = "feature/".into();
    settings.base_branch = blongo_protocol::BaseBranch::Custom {
        name: " develop ".into(),
    };
    settings.auto_fix_ci = false;
    let c = core.dispatch(Command::ProjectSetForge {
        project_id: project,
        settings: settings.clone(),
    });
    core.ok(&c).await;
    core.shutdown();
    let (core, shell) = TestCore::start(&dir);
    let p = shell.projects.iter().find(|p| p.id == project).unwrap();
    assert_eq!(p.forge.branch_prefix, "feature/");
    assert_eq!(
        p.forge.base_branch,
        blongo_protocol::BaseBranch::Custom {
            name: "develop".into()
        }
    );
    assert!(!p.forge.auto_fix_ci);
    core.shutdown();
}

/// A clone of the fake remote to push commits to it as someone else.
fn other_clone(dir: &Path) -> PathBuf {
    let other = dir.join("other");
    git(
        dir,
        &[
            "clone",
            "--quiet",
            &dir.join("remote.git").to_string_lossy(),
            &other.to_string_lossy(),
        ],
    );
    git(&other, &["config", "user.name", "Other"]);
    git(&other, &["config", "user.email", "other@localhost"]);
    git(&other, &["config", "commit.gpgsign", "false"]);
    other
}

fn commit_file(repo: &Path, name: &str, message: &str) {
    std::fs::write(repo.join(name), format!("{message}\n")).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", message]);
}

impl TestCore {
    async fn titled_thread(&mut self, project_id: ProjectId, title: &str) -> Thread {
        let thread_id = ThreadId::new();
        self.dispatch(Command::ThreadCreate {
            thread_id,
            project_id,
            title: title.into(),
            provider: ProviderKind::Codex,
            model: None,
            worktree: true,
            parent_thread_id: None,
        });
        self.until(|e| match e {
            CoreEvent::Event(ev) => match &ev.kind {
                EventKind::ThreadCreated { thread } if thread.id == thread_id => {
                    Some((**thread).clone())
                }
                _ => None,
            },
            _ => None,
        })
        .await
    }

    async fn prepare(&mut self, thread_id: ThreadId) -> blongo_protocol::PrPrepare {
        match self.query(Query::PrPrepare { thread_id }).await {
            Ok(QueryReply::PrPrepare(p)) => *p,
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn creates_a_pull_request_from_the_worktree() {
    let dir = temp_dir("forge-create");
    github_project(&dir);
    // GitHub's main moves on after the project last fetched.
    let other = other_clone(&dir);
    commit_file(&other, "upstream.txt", "Upstream change");
    git(&other, &["push", "--quiet", "origin", "main"]);
    let (mut core, gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Fix the parser").await;
    let wt = thread.worktree.clone().unwrap();
    let worktree = PathBuf::from(&wt.path);
    assert!(wt.branch.starts_with("blongo/"), "{}", wt.branch);
    assert!(
        worktree.join("upstream.txt").exists(),
        "the worktree starts from GitHub's main, not the project's HEAD"
    );
    assert_eq!(
        git(&worktree, &["rev-parse", "HEAD"]),
        git(&other, &["rev-parse", "HEAD"])
    );
    // No upstream: pushing later goes to its own name.
    assert!(
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "@{upstream}"])
            .current_dir(&worktree)
            .output()
            .unwrap()
            .stdout
            .is_empty()
    );

    std::fs::write(worktree.join("parser.rs"), "fn parse() {}\n").unwrap();
    let prep = core.prepare(thread.id).await;
    assert_eq!(prep.repo, "acme/widgets");
    assert_eq!(prep.base, "main");
    assert_eq!(prep.branch, wt.branch);
    assert!(!prep.pushed);
    assert_eq!(prep.suggested_branch, "blongo/fix-the-parser");
    assert_eq!(prep.uncommitted, vec!["parser.rs".to_owned()]);
    assert!(prep.commits.is_empty());
    assert_eq!(prep.title, "Fix the parser");
    assert!(prep.can_push);
    assert!(prep.draft_prompt.contains("parser.rs"));

    let mut request = blongo_protocol::PrCreateRequest {
        title: "Fix the parser".into(),
        body: "Parses again.".into(),
        base: "main".into(),
        draft: true,
        branch: prep.suggested_branch.clone(),
        commit_message: None,
    };
    let err = core
        .query(Query::PrCreate {
            thread_id: thread.id,
            request: request.clone(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("not committed"), "{err}");
    for (bad, want) in [("main", "same"), ("-x", "not a branch name")] {
        let mut r = request.clone();
        r.branch = bad.into();
        let err = core
            .query(Query::PrCreate {
                thread_id: thread.id,
                request: r,
            })
            .await
            .unwrap_err();
        assert!(err.contains(want), "{bad}: {err}");
    }

    // A delegated task works in the same worktree: it follows the rename.
    let child_id = ThreadId::new();
    core.dispatch(Command::ThreadCreate {
        thread_id: child_id,
        project_id: project,
        title: "Helper".into(),
        provider: ProviderKind::Codex,
        model: None,
        worktree: false,
        parent_thread_id: Some(thread.id),
    });
    core.until(|e| match e {
        CoreEvent::Event(ev) => match &ev.kind {
            EventKind::ThreadCreated { thread } if thread.id == child_id => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;
    let mut child_renamed = false;
    request.commit_message = Some("Fix the parser".into());
    let id = core.send_query(Query::PrCreate {
        thread_id: thread.id,
        request: request.clone(),
    });
    let mut renamed = None;
    let mut linked = None;
    let reply = core
        .until(|e| match e {
            CoreEvent::Reply { id: got, result } if *got == id => Some(result.clone()),
            CoreEvent::Event(ev) => {
                match &ev.kind {
                    EventKind::ThreadBranchRenamed { thread_id, branch }
                        if *thread_id == thread.id =>
                    {
                        renamed = Some(branch.clone());
                    }
                    EventKind::ThreadBranchRenamed { thread_id, branch }
                        if *thread_id == child_id =>
                    {
                        child_renamed = branch == "blongo/fix-the-parser";
                    }
                    EventKind::ThreadPrLinked { thread_id, pr, .. } if *thread_id == thread.id => {
                        linked = pr.clone();
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        })
        .await;
    assert_eq!(reply, Ok(QueryReply::Done("acme/widgets#1 created".into())));
    assert_eq!(renamed.as_deref(), Some("blongo/fix-the-parser"));
    assert!(child_renamed, "the delegated thread follows the rename");
    let link = linked.unwrap();
    assert_eq!(
        (link.number, link.head_branch.as_str()),
        (1, "blongo/fix-the-parser")
    );
    assert!(!link.read_only);
    // Committed and pushed under the new name.
    let bare = dir.join("remote.git");
    assert_eq!(
        git(&bare, &["rev-parse", "refs/heads/blongo/fix-the-parser"]),
        git(&worktree, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        git(&worktree, &["log", "-1", "--format=%s"]),
        "Fix the parser"
    );
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    let post = gh
        .log()
        .await
        .into_iter()
        .find(|r| r["method"] == "POST" && r["path"] == "/repos/acme/widgets/pulls")
        .expect("the create request");
    assert_eq!(post["body"]["head"], "blongo/fix-the-parser");
    assert_eq!(post["body"]["base"], "main");
    assert_eq!(post["body"]["draft"], true);
    assert_eq!(post["body"]["body"], "Parses again.");

    // Linked: a second create is refused.
    let err = core
        .query(Query::PrCreate {
            thread_id: thread.id,
            request: request.clone(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("linked already"), "{err}");

    // Someone else pushes to the branch: Blongo never forces.
    git(&other, &["fetch", "--quiet", "origin"]);
    git(&other, &["switch", "--quiet", "blongo/fix-the-parser"]);
    commit_file(&other, "theirs.txt", "Their change");
    git(
        &other,
        &["push", "--quiet", "origin", "blongo/fix-the-parser"],
    );
    let theirs = git(&other, &["rev-parse", "HEAD"]);
    commit_file(&worktree, "ours.txt", "Our change");
    let err = core
        .query(Query::PrPush {
            thread_id: thread.id,
        })
        .await
        .unwrap_err();
    assert!(err.contains("never force-pushes"), "{err}");
    assert_eq!(
        git(&bare, &["rev-parse", "refs/heads/blongo/fix-the-parser"]),
        theirs
    );
    // Brought in, it pushes.
    git(&worktree, &["pull", "--quiet", "--no-rebase", "--no-edit"]);
    let reply = core
        .query(Query::PrPush {
            thread_id: thread.id,
        })
        .await;
    assert_eq!(
        reply,
        Ok(QueryReply::Done("pushed blongo/fix-the-parser".into()))
    );
    core.shutdown();

    // The new branch name survives a restart.
    let (core, shell) = start_again(&dir, &gh);
    let t = shell.threads.iter().find(|t| t.id == thread.id).unwrap();
    assert_eq!(
        t.worktree.as_ref().map(|w| w.branch.as_str()),
        Some("blongo/fix-the-parser")
    );
    assert_eq!(t.pr.as_ref().map(|p| p.number), Some(1));
    let child = shell.threads.iter().find(|t| t.id == child_id).unwrap();
    assert_eq!(
        child.worktree.as_ref().map(|w| w.branch.as_str()),
        Some("blongo/fix-the-parser")
    );
    core.shutdown();
}

#[tokio::test]
async fn an_open_pull_request_is_linked_on_retry() {
    let dir = temp_dir("forge-retry");
    github_project(&dir);
    let (mut core, gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Retry").await;
    let wt = thread.worktree.clone().unwrap();
    let worktree = PathBuf::from(&wt.path);
    commit_file(&worktree, "a.txt", "Add a");
    // Pushed and opened by hand already; the name stays.
    git(&worktree, &["push", "--quiet", "origin", &wt.branch]);
    let head = git(&worktree, &["rev-parse", "HEAD"]);
    gh.pull(json!({"number": 9, "title": "By hand", "head": wt.branch, "head_sha": head}))
        .await;
    let prep = core.prepare(thread.id).await;
    assert!(prep.pushed);
    assert_eq!(prep.suggested_branch, wt.branch);
    assert_eq!(prep.commits, vec!["Add a".to_owned()]);
    let mut request = blongo_protocol::PrCreateRequest {
        title: "Retry".into(),
        body: String::new(),
        base: "main".into(),
        draft: false,
        branch: "blongo/renamed".into(),
        commit_message: None,
    };
    let err = core
        .query(Query::PrCreate {
            thread_id: thread.id,
            request: request.clone(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("keeps its name"), "{err}");
    request.branch = wt.branch.clone();
    let reply = core
        .query(Query::PrCreate {
            thread_id: thread.id,
            request,
        })
        .await;
    assert_eq!(reply, Ok(QueryReply::Done("acme/widgets#9 created".into())));
    core.shutdown();
}

#[tokio::test]
async fn worktrees_start_from_the_project_base_branch() {
    let dir = temp_dir("forge-base");
    github_project(&dir);
    let other = other_clone(&dir);
    git(&other, &["switch", "--quiet", "-c", "develop"]);
    commit_file(&other, "develop.txt", "On develop");
    git(&other, &["push", "--quiet", "origin", "develop"]);
    let (mut core, _gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let c = core.dispatch(Command::ProjectSetForge {
        project_id: project,
        settings: blongo_protocol::ForgeSettings {
            base_branch: blongo_protocol::BaseBranch::Custom {
                name: "develop".into(),
            },
            branch_prefix: "feature/".into(),
            ..Default::default()
        },
    });
    core.ok(&c).await;
    let thread = core.titled_thread(project, "On develop").await;
    let wt = thread.worktree.clone().unwrap();
    assert!(wt.branch.starts_with("feature/"), "{}", wt.branch);
    assert!(PathBuf::from(&wt.path).join("develop.txt").exists());
    let prep = core.prepare(thread.id).await;
    assert_eq!(prep.base, "develop");
    assert_eq!(prep.suggested_branch, "feature/on-develop");

    // A base the remote lacks: the project's HEAD, and the thread says so.
    let c = core.dispatch(Command::ProjectSetForge {
        project_id: project,
        settings: blongo_protocol::ForgeSettings {
            base_branch: blongo_protocol::BaseBranch::Custom {
                name: "nope".into(),
            },
            ..Default::default()
        },
    });
    core.ok(&c).await;
    let thread = core.titled_thread(project, "Missing base").await;
    let notice = core
        .snapshot(thread.id)
        .await
        .items
        .iter()
        .find_map(|i| match &i.kind {
            blongo_protocol::ItemKind::SystemNotice { message } => Some(message.clone()),
            _ => None,
        });
    assert!(
        notice.as_deref().is_some_and(|m| m.contains("origin/nope")),
        "{notice:?}"
    );
    core.shutdown();
}

#[tokio::test]
async fn the_agent_drafts_the_pull_request() {
    let dir = temp_dir("forge-draft");
    github_project(&dir);
    let (mut core, _gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Draft").await;
    let reply = core
        .query(Query::PrDraft {
            thread_id: thread.id,
            prompt: "echo: ```json\n{\"title\": \"Drafted title\", \"body\": \"Why\", \
                     \"commit_message\": \"Commit it\"}\n```"
                .into(),
        })
        .await;
    assert_eq!(
        reply,
        Ok(QueryReply::PrDraft(blongo_protocol::PrDraft {
            title: "Drafted title".into(),
            body: "Why".into(),
            commit_message: Some("Commit it".into()),
        }))
    );
    let err = core
        .query(Query::PrDraft {
            thread_id: thread.id,
            prompt: "echo: nothing useful".into(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("no draft"), "{err}");
    // An interrupted turn answers with an error.
    let id = core.send_query(Query::PrDraft {
        thread_id: thread.id,
        prompt: "loop".into(),
    });
    core.added_item(|i| i.thread_id == thread.id && &*i.text == "loop")
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    core.dispatch(Command::RunInterrupt {
        thread_id: thread.id,
    });
    let err = core.replies(&[id]).await.remove(0).unwrap_err();
    assert!(err.contains("did not answer"), "{err}");
    let err = core
        .query(Query::PrDraft {
            thread_id: ThreadId::new(),
            prompt: "hi".into(),
        })
        .await
        .unwrap_err();
    assert!(!err.is_empty());
    core.shutdown();
}

#[tokio::test]
async fn a_branch_name_taken_on_github_is_refused() {
    let dir = temp_dir("forge-taken");
    github_project(&dir);
    // A teammate's branch has the name the title suggests.
    let other = other_clone(&dir);
    git(&other, &["switch", "--quiet", "-c", "blongo/fix-readme"]);
    commit_file(&other, "theirs.txt", "Their work");
    git(&other, &["push", "--quiet", "origin", "blongo/fix-readme"]);
    let theirs = git(&other, &["rev-parse", "HEAD"]);
    let (mut core, gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Fix README").await;
    let wt = thread.worktree.clone().unwrap();
    let worktree = PathBuf::from(&wt.path);
    commit_file(&worktree, "mine.txt", "My work");
    let prep = core.prepare(thread.id).await;
    assert_eq!(prep.suggested_branch, "blongo/fix-readme");
    let request = blongo_protocol::PrCreateRequest {
        title: "Fix README".into(),
        body: String::new(),
        base: "main".into(),
        draft: false,
        branch: prep.suggested_branch.clone(),
        commit_message: None,
    };
    let err = core
        .query(Query::PrCreate {
            thread_id: thread.id,
            request,
        })
        .await
        .unwrap_err();
    assert!(err.contains("exists on GitHub"), "{err}");
    // Nothing pushed, nothing renamed, no pull request.
    let bare = dir.join("remote.git");
    assert_eq!(
        git(&bare, &["rev-parse", "refs/heads/blongo/fix-readme"]),
        theirs
    );
    assert_eq!(git(&worktree, &["branch", "--show-current"]), wt.branch);
    assert!(
        !gh.log()
            .await
            .iter()
            .any(|r| r["method"] == "POST" && r["path"] == "/repos/acme/widgets/pulls")
    );
    core.shutdown();
}

#[tokio::test]
async fn a_failed_push_keeps_the_rename_and_a_retry_finishes() {
    let dir = temp_dir("forge-push-retry");
    github_project(&dir);
    // The remote refuses pushes while a marker file exists.
    let bare = dir.join("remote.git");
    let marker = dir.join("refuse");
    let hook = bare.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif [ -e '{}' ]; then echo refused >&2; exit 1; fi\n",
            marker.display()
        ),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .args(["+x", &hook.to_string_lossy()])
        .status()
        .unwrap();
    std::fs::write(&marker, "").unwrap();
    let (mut core, _gh) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Retry push").await;
    let worktree = PathBuf::from(&thread.worktree.as_ref().unwrap().path);
    commit_file(&worktree, "a.txt", "Work");
    let request = blongo_protocol::PrCreateRequest {
        title: "Retry push".into(),
        body: String::new(),
        base: "main".into(),
        draft: false,
        branch: "blongo/retry-push".into(),
        commit_message: None,
    };
    let id = core.send_query(Query::PrCreate {
        thread_id: thread.id,
        request: request.clone(),
    });
    let mut renamed = false;
    let reply = core
        .until(|e| match e {
            CoreEvent::Reply { id: got, result } if *got == id => Some(result.clone()),
            CoreEvent::Event(ev) => {
                if let EventKind::ThreadBranchRenamed { thread_id, .. } = &ev.kind {
                    renamed |= *thread_id == thread.id;
                }
                None
            }
            _ => None,
        })
        .await;
    let err = reply.unwrap_err();
    assert!(err.starts_with("Pushing failed"), "{err}");
    assert!(renamed, "the rename is kept although the push failed");
    std::fs::remove_file(&marker).unwrap();
    let reply = core
        .query(Query::PrCreate {
            thread_id: thread.id,
            request,
        })
        .await;
    assert_eq!(reply, Ok(QueryReply::Done("acme/widgets#1 created".into())));
    core.shutdown();
}

#[tokio::test]
async fn worktrees_without_github_start_from_head() {
    // Integration off: the project's HEAD, as before G3.
    let dir = temp_dir("forge-off");
    github_project(&dir);
    let other = other_clone(&dir);
    commit_file(&other, "upstream.txt", "Upstream");
    git(&other, &["push", "--quiet", "origin", "main"]);
    let (mut core, _) = TestCore::start_with(&dir, |c| c.forge = false);
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Off").await;
    let wt = thread.worktree.clone().unwrap();
    assert!(!PathBuf::from(&wt.path).join("upstream.txt").exists());
    core.shutdown();

    // A remote that is not GitHub (a path) with no HEAD recorded: the
    // project's HEAD, without a notice.
    let dir = temp_dir("forge-plain-remote");
    let bare = dir.join("plain.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
    let project_dir = dir.join("project");
    git(&project_dir, &["init", "--quiet", "-b", "main"]);
    git(&project_dir, &["config", "user.name", "Test"]);
    git(&project_dir, &["config", "user.email", "test@localhost"]);
    git(
        &project_dir,
        &["remote", "add", "origin", &bare.to_string_lossy()],
    );
    commit_file(&project_dir, "local.txt", "Local only");
    let (mut core, _) = start(&dir).await;
    let project = core.project(&dir).await;
    let thread = core.titled_thread(project, "Plain").await;
    let wt = thread.worktree.clone().unwrap();
    assert!(PathBuf::from(&wt.path).join("local.txt").exists());
    let snapshot = core.snapshot(thread.id).await;
    assert!(
        !snapshot
            .items
            .iter()
            .any(|i| matches!(i.kind, blongo_protocol::ItemKind::SystemNotice { .. })),
        "no notice for a folder without a GitHub base"
    );

    // A draft for an archived thread is refused.
    let c = core.dispatch(Command::ThreadArchive {
        thread_id: thread.id,
    });
    core.ok(&c).await;
    let err = core
        .query(Query::PrDraft {
            thread_id: thread.id,
            prompt: "hi".into(),
        })
        .await
        .unwrap_err();
    assert!(err.contains("archived"), "{err}");
    core.shutdown();
}
