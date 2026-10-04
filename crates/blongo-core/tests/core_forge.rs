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
