//! Starting a thread from a pull request, an issue or a branch ("New
//! thread from…" and the review inbox's "Open as thread").
//!
//! GitHub is asked and the branch fetched in a job; the thread is then
//! created like any worktree thread, its worktree on that branch instead
//! of a new one. A pull request's thread links it (read-only when it comes
//! from a fork); an issue's thread gets a branch named after the issue,
//! the issue in its composer, and `Closes #n` in its pull request. Issue
//! and pull request text is never sent by itself: the user reads the
//! draft first.

use std::path::{Path, PathBuf};

use blongo_forge::branch;
use blongo_forge::github::GhError;
use blongo_forge::remote::{self, RepoRef};
use blongo_protocol::{
    BranchCandidate, ForgeCandidates, PrState, ThreadOpened, ThreadSource, branch_slug,
    parse_issue_ref, parse_pr_ref,
};

use super::forge::{BasePlan, ForgeCtx, branch_name_ok};
use super::*;

/// Most branches offered.
const MAX_BRANCHES: usize = 30;
/// Longest issue body put in the draft.
const MAX_DRAFT_BODY: usize = 8_000;
/// Longest thread title taken from a pull request or issue.
const MAX_TITLE_CHARS: usize = 80;

/// How a new thread's worktree gets its branch.
pub(super) enum Checkout {
    /// A new branch from the base (`None`: the project's HEAD).
    New(Option<BasePlan>),
    /// A new branch at this ref.
    At(String),
    /// The existing local branch.
    Existing,
}

/// Where a thread being opened starts, taken when its worktree is
/// prepared.
pub(super) struct Source {
    pub branch: String,
    pub start: Start,
}

pub(super) enum Start {
    /// From the project's base branch.
    Base,
    At(String),
    Existing,
}

/// A `ThreadFrom` query waiting for its thread to be created.
pub(super) struct Opening {
    thread_id: ThreadId,
    /// Linked once the thread exists (`#n`).
    link: Option<String>,
    issue: Option<u64>,
    draft: Option<String>,
    message: String,
}

pub(super) enum OpenDone {
    Candidates {
        query: QueryId,
        result: Result<ForgeCandidates, String>,
    },
    Resolved {
        query: QueryId,
        thread_id: ThreadId,
        provider: ProviderKind,
        model: Option<String>,
        result: Result<Resolved, String>,
    },
}

pub(super) struct Resolved {
    project_id: ProjectId,
    title: String,
    source: Source,
    link: Option<String>,
    issue: Option<u64>,
    draft: Option<String>,
    message: String,
}

/// A project a `ThreadFrom` may land in.
struct Place {
    project_id: ProjectId,
    path: PathBuf,
    prefix: String,
}

/// The project's folder as GitHub knows it.
struct Repo {
    place: Place,
    remote_name: Option<String>,
    repo: Option<RepoRef>,
}

impl Orchestrator {
    /// The issue thread `thread_id` was started from.
    pub(super) fn thread_issue(&self, thread_id: ThreadId) -> Option<u64> {
        let (text, _) = self
            .store
            .forge_cache(&format!("issue:{thread_id}"))
            .ok()
            .flatten()?;
        text.parse().ok()
    }

    /// `Query::ForgeCandidates`.
    pub(super) fn forge_candidates(&mut self, id: QueryId, project_id: ProjectId) {
        let Some(project) = self.projects.get(&project_id) else {
            return self.emit(CoreEvent::Reply {
                id,
                result: Err("unknown project".into()),
            });
        };
        let cwd = PathBuf::from(&project.path);
        let ctx = self.config.forge.then(|| self.forge_ctx());
        self.spawn_job(Key::None, async move {
            let result = candidates(ctx.as_ref(), &cwd).await;
            JobDone::ForgeOpen(Box::new(OpenDone::Candidates { query: id, result }))
        });
    }

    /// `Query::ThreadFrom`.
    pub(super) fn thread_from(
        &mut self,
        id: QueryId,
        project_id: Option<ProjectId>,
        thread_id: ThreadId,
        source: ThreadSource,
        provider: ProviderKind,
        model: Option<String>,
    ) {
        let job = (|| {
            if self.threads.contains_key(&thread_id) {
                return Err("thread already exists".to_owned());
            }
            let wanted = match &source {
                ThreadSource::Branch { name } => {
                    if !branch_name_ok(name.trim(), false) {
                        return Err(format!("\"{}\" is not a branch name", name.trim()));
                    }
                    None
                }
                ThreadSource::Pull { reference } => Some(
                    parse_pr_ref(reference)
                        .ok_or("enter a pull request URL, owner/name#number or #number")?,
                ),
                ThreadSource::Issue { reference } => Some(
                    parse_issue_ref(reference)
                        .ok_or("enter an issue URL, owner/name#number or #number")?,
                ),
            };
            if wanted.is_some() && !self.config.forge {
                return Err("GitHub integration is turned off".into());
            }
            let places: Vec<Place> = match project_id {
                Some(pid) => {
                    let p = self.projects.get(&pid).ok_or("unknown project")?;
                    vec![Place {
                        project_id: pid,
                        path: PathBuf::from(&p.path),
                        prefix: p.forge.branch_prefix.clone(),
                    }]
                }
                None => {
                    if !wanted.as_ref().is_some_and(|(_, repo, _)| repo.is_some()) {
                        return Err("choose a project to start the thread in".into());
                    }
                    let mut all: Vec<&Project> = self.projects.values().collect();
                    all.sort_by_key(|p| p.created_at);
                    all.into_iter()
                        .map(|p| Place {
                            project_id: p.id,
                            path: PathBuf::from(&p.path),
                            prefix: p.forge.branch_prefix.clone(),
                        })
                        .collect()
                }
            };
            Ok((places, wanted))
        })();
        let (places, wanted) = match job {
            Ok(v) => v,
            Err(err) => {
                return self.emit(CoreEvent::Reply {
                    id,
                    result: Err(err),
                });
            }
        };
        let ctx = self.forge_ctx();
        let named = project_id.is_some();
        self.spawn_job(Key::None, async move {
            let result = resolve(&ctx, places, named, source, wanted).await;
            JobDone::ForgeOpen(Box::new(OpenDone::Resolved {
                query: id,
                thread_id,
                provider,
                model,
                result,
            }))
        });
    }

    pub(super) fn open_done(&mut self, done: OpenDone) {
        match done {
            OpenDone::Candidates { query, result } => self.emit(CoreEvent::Reply {
                id: query,
                result: result.map(|c| QueryReply::Candidates(Box::new(c))),
            }),
            OpenDone::Resolved {
                query,
                thread_id,
                provider,
                model,
                result,
            } => {
                let resolved = result.and_then(|r| {
                    if self.threads.contains_key(&thread_id) {
                        return Err("thread already exists".into());
                    }
                    if !self.projects.contains_key(&r.project_id) {
                        return Err("unknown project".into());
                    }
                    Ok(r)
                });
                let r = match resolved {
                    Ok(r) => r,
                    Err(err) => {
                        return self.emit(CoreEvent::Reply {
                            id: query,
                            result: Err(err),
                        });
                    }
                };
                self.forge.sources.insert(thread_id, r.source);
                self.forge.opening.insert(
                    query,
                    Opening {
                        thread_id,
                        link: r.link,
                        issue: r.issue,
                        draft: r.draft,
                        message: r.message,
                    },
                );
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    Command::ThreadCreate {
                        thread_id,
                        project_id: r.project_id,
                        title: r.title,
                        provider,
                        model,
                        worktree: true,
                        parent_thread_id: None,
                    },
                    Reply::Query(query),
                )));
            }
        }
    }

    /// How a new worktree thread's branch is made: from its source when
    /// it is being opened from one, else a new branch from the base.
    pub(super) fn worktree_checkout(
        &mut self,
        thread_id: ThreadId,
        project_id: ProjectId,
        generated: String,
    ) -> (String, Checkout) {
        let source = self.forge.sources.remove(&thread_id);
        let base = || {
            self.projects
                .get(&project_id)
                .and_then(|p| self.base_plan(p))
        };
        match source {
            None => (generated, Checkout::New(base())),
            Some(Source { branch, start }) => {
                let checkout = match start {
                    Start::Base => Checkout::New(base()),
                    Start::At(r) => Checkout::At(r),
                    Start::Existing => Checkout::Existing,
                };
                (branch, checkout)
            }
        }
    }

    /// A `ThreadFrom` query's thread was created: link its pull request,
    /// remember its issue, answer. `false`: `id` is no such query.
    pub(super) fn opened(&mut self, id: QueryId) -> bool {
        let Some(o) = self.forge.opening.remove(&id) else {
            return false;
        };
        if let Some(n) = o.issue
            && let Err(err) = self
                .store
                .set_forge_cache(&format!("issue:{}", o.thread_id), &n.to_string())
        {
            eprintln!("blongo-core: forge cache: {err:#}");
        }
        if let Some(pr) = o.link {
            self.deferred.push_back(Deferred::Dispatch(Pending::new(
                Command::ThreadLinkPr {
                    thread_id: o.thread_id,
                    pr,
                },
                Reply::Internal,
            )));
        }
        self.emit(CoreEvent::Reply {
            id,
            result: Ok(QueryReply::ThreadOpened(ThreadOpened {
                thread_id: o.thread_id,
                draft: o.draft,
                message: o.message,
            })),
        });
        true
    }

    /// A `ThreadFrom` query's thread was refused.
    pub(super) fn open_refused(&mut self, id: QueryId) {
        if let Some(o) = self.forge.opening.remove(&id) {
            self.forge.sources.remove(&o.thread_id);
        }
    }
}

/// What "New thread from…" offers. Branches come from git, so they are
/// offered even when GitHub cannot be asked.
async fn candidates(ctx: Option<&ForgeCtx>, cwd: &Path) -> Result<ForgeCandidates, String> {
    if blongo_git::work_tree_root(cwd).await.is_none() {
        return Err(format!("{} is not in a git repository", cwd.display()));
    }
    let remote_name = remote::remote_name(cwd).await;
    let busy = branch::checked_out(cwd).await;
    let mut out = ForgeCandidates {
        branches: branch::recent_branches(cwd, remote_name.as_deref(), 200)
            .await
            .into_iter()
            .filter(|(name, _)| !busy.contains_key(name))
            .take(MAX_BRANCHES)
            .map(|(name, remote_only)| BranchCandidate { name, remote_only })
            .collect(),
        ..ForgeCandidates::default()
    };
    let Some(ctx) = ctx else {
        out.problem = Some("GitHub integration is turned off".into());
        return Ok(out);
    };
    let Some(repo) = remote::remote_url(cwd)
        .await
        .and_then(|u| RepoRef::parse(&u))
    else {
        out.problem = Some("this folder's remote is not a GitHub repository".into());
        return Ok(out);
    };
    out.repo = repo.full_name();
    let lists = async {
        let gh = ctx.client(&repo, ctx.token(&repo.host).await?);
        let login = gh.login().await.ok();
        let fail = |e: GhError| ctx.fail(&repo.host, e);
        let (pulls, issues) = tokio::join!(
            gh.open_pulls(&repo, login.as_deref()),
            gh.open_issues(&repo, login.as_deref())
        );
        Ok::<_, String>((pulls.map_err(fail)?, issues.map_err(fail)?))
    };
    match lists.await {
        Ok((mut pulls, mut issues)) => {
            // The user's own first; GitHub's order (recently updated)
            // otherwise.
            pulls.sort_by_key(|c| !c.mine);
            issues.sort_by_key(|c| !c.mine);
            out.pulls = pulls;
            out.issues = issues;
        }
        Err(err) => out.problem = Some(err),
    }
    Ok(out)
}

/// Find the project (`named`: the one in `places`), ask GitHub and
/// fetch the branch.
async fn resolve(
    ctx: &ForgeCtx,
    places: Vec<Place>,
    named: bool,
    source: ThreadSource,
    wanted: Option<(Option<String>, Option<String>, u64)>,
) -> Result<Resolved, String> {
    let (host, full, number) = match &wanted {
        Some((h, r, n)) => (h.clone(), r.clone(), *n),
        None => (None, None, 0),
    };
    let single = named;
    let mut found = None;
    for place in places {
        if blongo_git::work_tree_root(&place.path).await.is_none() {
            if single {
                return Err(format!(
                    "{} is not in a git repository",
                    place.path.display()
                ));
            }
            continue;
        }
        let remote_name = remote::remote_name(&place.path).await;
        let repo = remote::remote_url(&place.path)
            .await
            .and_then(|u| RepoRef::parse(&u));
        let matches = match (&full, &repo) {
            (None, _) => true,
            (Some(full), Some(repo)) => {
                full.eq_ignore_ascii_case(&repo.full_name())
                    && host
                        .as_ref()
                        .is_none_or(|h| h.eq_ignore_ascii_case(&repo.host))
            }
            (Some(_), None) => false,
        };
        if matches {
            found = Some(Repo {
                place,
                remote_name,
                repo,
            });
            break;
        }
        if single {
            return Err(match repo {
                Some(repo) => format!(
                    "{} is not this project's repository ({})",
                    full.unwrap_or_default(),
                    repo.full_name()
                ),
                None => "this project's folder has no GitHub remote".into(),
            });
        }
    }
    let Repo {
        place,
        remote_name,
        repo,
    } = found.ok_or_else(|| {
        format!(
            "no project here works on {}; add its folder as a project first",
            full.clone().unwrap_or_default()
        )
    })?;
    let cwd = place.path.as_path();
    let busy = branch::checked_out(cwd).await;
    let taken = |name: &str| -> Result<(), String> {
        match busy.get(name) {
            Some(at) => Err(format!(
                "{name} is checked out in {at}; work there, or pick another branch"
            )),
            None => Ok(()),
        }
    };
    match source {
        ThreadSource::Branch { name } => {
            let name = name.trim().to_owned();
            taken(&name)?;
            let start = if branch::has_local_branch(cwd, &name).await {
                Start::Existing
            } else {
                let r = remote_name.ok_or_else(|| format!("there is no branch {name}"))?;
                branch::fetch(cwd, &r, &name, branch::FETCH_TIMEOUT)
                    .await
                    .map_err(|_| format!("there is no branch {name} here or on {r}"))?;
                Start::At(format!("refs/remotes/{r}/{name}"))
            };
            Ok(Resolved {
                project_id: place.project_id,
                title: name.clone(),
                message: format!("Started a thread on {name}"),
                source: Source {
                    branch: name,
                    start,
                },
                link: None,
                issue: None,
                draft: None,
            })
        }
        ThreadSource::Pull { .. } => {
            let (repo, r) = github_parts(repo, remote_name)?;
            let gh = ctx.client(&repo, ctx.token(&repo.host).await?);
            let info = gh
                .repo_info(&repo)
                .await
                .map_err(|e| ctx.fail(&repo.host, e))?;
            let pull = gh.pull(&repo, number).await.map_err(|e| match e {
                GhError::NotFound => format!("{}#{number} was not found", repo.full_name()),
                e => ctx.fail(&repo.host, e),
            })?;
            match pull.state {
                PrState::Merged => return Err(format!("#{number} is merged already")),
                PrState::Closed => return Err(format!("#{number} is closed")),
                PrState::Open | PrState::Draft => {}
            }
            let own = pull
                .head_repo
                .as_deref()
                .is_some_and(|h| h.eq_ignore_ascii_case(&repo.full_name()))
                && branch_name_ok(&pull.head_branch, false);
            let (name, start) = if own {
                let name = pull.head_branch.clone();
                taken(&name)?;
                // Also brings the remote-tracking branch up to date, so
                // the PR tab compares with GitHub's.
                branch::fetch(cwd, &r, &name, branch::FETCH_TIMEOUT).await?;
                let start = if branch::has_local_branch(cwd, &name).await {
                    Start::Existing
                } else {
                    Start::At(format!("refs/remotes/{r}/{name}"))
                };
                (name, start)
            } else {
                let name = format!("{}pr-{number}", place.prefix);
                if !branch_name_ok(&name, false) {
                    return Err(format!("\"{name}\" is not a branch name"));
                }
                taken(&name)?;
                let at = branch::fetch_pull(cwd, &r, number).await?;
                let start = if branch::has_local_branch(cwd, &name).await {
                    Start::Existing
                } else {
                    Start::At(at)
                };
                (name, start)
            };
            let read_only = !own || !info.can_push;
            Ok(Resolved {
                project_id: place.project_id,
                title: short_title(&pull.title, &format!("#{number}")),
                message: format!(
                    "{}#{number} opened in a new worktree on {name}{}",
                    repo.full_name(),
                    if read_only {
                        " (read-only: Blongo will not push to it)"
                    } else {
                        ""
                    }
                ),
                source: Source {
                    branch: name,
                    start,
                },
                link: Some(format!("#{number}")),
                issue: None,
                draft: None,
            })
        }
        ThreadSource::Issue { .. } => {
            let (repo, r) = github_parts(repo, remote_name)?;
            let gh = ctx.client(&repo, ctx.token(&repo.host).await?);
            let issue = gh.issue(&repo, number).await.map_err(|e| match e {
                GhError::NotFound => format!("{}#{number} was not found", repo.full_name()),
                e => ctx.fail(&repo.host, e),
            })?;
            if issue.is_pull {
                return Err(format!("#{number} is a pull request; open it as one"));
            }
            if !issue.open {
                return Err(format!("issue #{number} is closed"));
            }
            let slug = branch_slug(&issue.title);
            let stem = if slug.is_empty() {
                format!("{}{number}", place.prefix)
            } else {
                format!("{}{number}-{slug}", place.prefix)
            };
            let mut name = stem.clone();
            for i in 2..=10 {
                let free = !busy.contains_key(&name)
                    && !branch::has_local_branch(cwd, &name).await
                    && !branch::has_remote_branch(cwd, &r, &name).await;
                if free {
                    break;
                }
                if i == 10 {
                    return Err(format!("{stem} and its numbered names are all taken"));
                }
                name = format!("{stem}-{i}");
            }
            if !branch_name_ok(&name, false) {
                return Err(format!("\"{name}\" is not a branch name"));
            }
            let mut body = issue.body.trim().replace("\r\n", "\n");
            if body.len() > MAX_DRAFT_BODY {
                let mut end = MAX_DRAFT_BODY;
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                body.truncate(end);
                body.push('…');
            }
            let title = issue.title.replace(['\n', '\r'], " ");
            let mut draft = format!("Work on issue #{number}: {title}\n");
            if !body.is_empty() {
                draft.push_str(&format!("\n{body}\n"));
            }
            if issue.url.starts_with("https://") {
                draft.push_str(&format!("\n{}\n", issue.url));
            }
            Ok(Resolved {
                project_id: place.project_id,
                title: short_title(&issue.title, &format!("Issue #{number}")),
                message: format!("Started a thread for issue #{number} on {name}"),
                source: Source {
                    branch: name,
                    start: Start::Base,
                },
                link: None,
                issue: Some(number),
                draft: Some(draft),
            })
        }
    }
}

/// The repository and remote a pull request or issue needs.
fn github_parts(
    repo: Option<RepoRef>,
    remote_name: Option<String>,
) -> Result<(RepoRef, String), String> {
    let repo = repo.ok_or("this project's folder has no GitHub remote")?;
    let r = remote_name.ok_or("this project's folder has no git remote")?;
    Ok((repo, r))
}

/// One line of at most [`MAX_TITLE_CHARS`] (`fallback` when empty).
fn short_title(title: &str, fallback: &str) -> String {
    let line = title.replace(['\n', '\r'], " ");
    let line = line.trim();
    if line.is_empty() {
        return fallback.to_owned();
    }
    if line.chars().count() <= MAX_TITLE_CHARS {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(MAX_TITLE_CHARS - 1).collect();
    out.push('…');
    out
}
