//! Fixing failed checks of a thread's pull request.
//!
//! When a poll sees the checks of a new head commit fail (and the
//! project allows it), what CI said is fetched and sent to the thread's
//! agent as a queued message; when that turn ends, Blongo commits what it
//! changed and pushes (never forced). Attempts are counted per pull
//! request until its checks pass; after the project's limit Blongo stops
//! and says so. A fix asked for by hand (`Query::PrFix`) is never pushed
//! for the user and starts the count over.
//!
//! The PR tab also sends unresolved review comments (`Query::PrComments`)
//! and merges the base branch in, sending conflicts to the agent
//! (`Query::PrMergeBase`); those are never pushed for the user.
//!
//! The count lives in the forge cache (`autofix:<thread>`), so a restart
//! neither repeats a fix for the same commit nor forgets the limit.

use std::path::PathBuf;

use blongo_forge::branch;
use blongo_forge::github::FailedCheck;
use blongo_forge::remote::{self, RepoRef};
use blongo_protocol::{AutoFixInfo, ChecksState, Delivery, PrLink, PrState, PrStatus};

use super::forge::ForgeCtx;
use super::*;

/// Where a pull request's automatic fixes stand (persisted as
/// `<sha> <attempts> <stopped>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FixState {
    /// The head commit the last fix was sent for.
    sha: String,
    /// Fixes sent since the checks last passed.
    attempts: u32,
    /// The limit was reached; said once.
    stopped: bool,
}

impl FixState {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(' ');
        Some(Self {
            sha: parts.next()?.to_owned(),
            attempts: parts.next()?.parse().ok()?,
            stopped: parts.next()? == "1",
        })
    }

    fn to_text(&self) -> String {
        format!("{} {} {}", self.sha, self.attempts, u8::from(self.stopped))
    }
}

/// A fix turn on its way or running.
pub(super) struct Fix {
    thread_id: ThreadId,
    /// Blongo commits and pushes when the turn ends.
    automatic: bool,
    /// The failed checks' names, for the commit message.
    names: Vec<String>,
    /// The query that asked (a fix asked for by hand), answered once the
    /// message is in.
    query: Option<QueryId>,
    /// Another turn ran in the folder before this one ended: its work
    /// is not the fix's to push.
    tainted: bool,
}

/// What CI said, and why the fix must not be pushed for the user when it
/// was meant to be (the folder holds other work).
pub(super) struct Report {
    checks: Vec<FailedCheck>,
    hold: Option<String>,
}

/// A fix job ended.
pub(super) enum FixDone {
    /// What CI said was fetched: send it.
    Report {
        thread_id: ThreadId,
        sha: String,
        /// `None`: automatic; else the query that asked.
        query: Option<QueryId>,
        result: Result<Report, String>,
    },
    /// The fix turn's changes were committed and pushed (or not).
    Pushed {
        thread_id: ThreadId,
        result: Result<String, String>,
    },
    /// Review comments were read, or the base branch was merged in
    /// (`key` is held until now).
    Send {
        thread_id: ThreadId,
        query: QueryId,
        key: Key,
        result: Result<SendOutcome, String>,
    },
}

/// What to do with the user's ask once its job ended.
pub(super) enum SendOutcome {
    /// Send this to the agent.
    Prompt(String),
    /// Nothing for the agent; answer with this.
    Done(String),
}

impl Orchestrator {
    fn fix_key(thread_id: ThreadId) -> String {
        format!("autofix:{thread_id}")
    }

    fn fix_state(&self, thread_id: ThreadId) -> FixState {
        self.store
            .forge_cache(&Self::fix_key(thread_id))
            .ok()
            .flatten()
            .and_then(|(text, _)| FixState::parse(&text))
            .unwrap_or_default()
    }

    fn set_fix_state(&self, thread_id: ThreadId, state: &FixState) {
        if let Err(err) = self
            .store
            .set_forge_cache(&Self::fix_key(thread_id), &state.to_text())
        {
            eprintln!("blongo-core: forge cache: {err:#}");
        }
    }

    /// What the PR tab shows of automatic fixes (`None`: not possible
    /// for this thread).
    pub(super) fn auto_fix_info(&self, thread: &Thread) -> Option<AutoFixInfo> {
        let link = thread.pr.as_ref()?;
        if link.read_only || !forge::owns_branch(thread) {
            return None;
        }
        let project = self.projects.get(&thread.project_id)?;
        let state = self.fix_state(thread.id);
        Some(AutoFixInfo {
            enabled: project.forge.auto_fix_ci,
            attempts: state.attempts,
            max: project.forge.auto_fix_max,
            stopped: state.stopped,
            running: self.forge.fixing.contains(&thread.id),
        })
    }

    /// A thread's pull request status was committed: fix failed checks of
    /// a head commit not tried yet; passed checks start the count over.
    pub(super) fn consider_fix(&mut self, thread_id: ThreadId, status: &PrStatus) {
        let before = self
            .forge
            .seen
            .insert(thread_id, (status.head_sha.clone(), status.checks.state));
        if !self.config.forge || status.error.is_some() || status.head_sha.is_empty() {
            return;
        }
        if status.checks.state == ChecksState::Success {
            if self.fix_state(thread_id) != FixState::default() {
                let _ = self.store.delete_forge_cache(&Self::fix_key(thread_id));
            }
            return;
        }
        // A failure seen already (a repeated poll, a restart) or first
        // seen on linking an existing pull request is not news.
        let news = before
            .is_some_and(|(sha, state)| sha != status.head_sha || state != ChecksState::Failure);
        if status.checks.state != ChecksState::Failure
            || !news
            || !matches!(status.state, PrState::Open | PrState::Draft)
            || self.forge.fixing.contains(&thread_id)
        {
            return;
        }
        let Some(thread) = self.threads.get(&thread_id).filter(|t| !t.archived) else {
            return;
        };
        let Some(link) = thread.pr.clone().filter(|l| !l.read_only) else {
            return;
        };
        if !forge::owns_branch(thread) {
            return;
        }
        let Some(project) = self.projects.get(&thread.project_id) else {
            return;
        };
        if !project.forge.auto_fix_ci {
            return;
        }
        let max = project.forge.auto_fix_max;
        let cwd = PathBuf::from(thread.cwd(project));
        let mut state = self.fix_state(thread_id);
        if state.sha == status.head_sha {
            return;
        }
        // This is failing run `attempts + 1` in a row: the limit counts
        // runs, so the last one stops instead of sending another fix.
        if state.attempts + 1 >= max {
            if !state.stopped {
                state.stopped = true;
                self.set_fix_state(thread_id, &state);
                let message = format!(
                    "Automatic CI fixes stopped: the checks of {} failed {} times in a row \
                     ({} fix{} sent). Fix it by hand or ask the agent from the PR tab.",
                    link.label(),
                    state.attempts + 1,
                    state.attempts,
                    if state.attempts == 1 { "" } else { "es" }
                );
                self.thread_notice(thread_id, &message);
                self.emit(CoreEvent::Notice { message });
            }
            return;
        }
        state.sha = status.head_sha.clone();
        state.attempts += 1;
        // The limit was raised since it stopped.
        state.stopped = false;
        self.set_fix_state(thread_id, &state);
        self.spawn_fix_report(thread_id, link, cwd, status.head_sha.clone(), None);
    }

    /// `Query::PrFix`: send the failing checks to the agent now.
    pub(super) fn pr_fix(&mut self, id: QueryId, thread_id: ThreadId) {
        let job = (|| {
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let thread = self.live_thread(thread_id)?;
            let link = thread.pr.clone().ok_or("no pull request is linked")?;
            let status = thread
                .pr_status
                .as_ref()
                .ok_or("the checks are not known yet")?;
            if status.checks.state != ChecksState::Failure {
                return Err("no checks failed".into());
            }
            if self.forge.fixing.contains(&thread_id) {
                return Err("a fix is on its way already".into());
            }
            let project = self
                .projects
                .get(&thread.project_id)
                .ok_or("unknown project")?;
            Ok((
                link,
                PathBuf::from(thread.cwd(project)),
                status.head_sha.clone(),
            ))
        })();
        match job {
            Ok((link, cwd, sha)) => {
                // The user takes over: automatic fixes count from zero.
                let _ = self.store.delete_forge_cache(&Self::fix_key(thread_id));
                self.spawn_fix_report(thread_id, link, cwd, sha, Some(id));
            }
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    fn spawn_fix_report(
        &mut self,
        thread_id: ThreadId,
        link: PrLink,
        cwd: PathBuf,
        sha: String,
        query: Option<QueryId>,
    ) {
        self.forge.fixing.insert(thread_id);
        let ctx = self.forge_ctx();
        self.spawn_job(Key::None, async move {
            let result = fix_report(&ctx, &link, &cwd, &sha, query.is_none()).await;
            JobDone::ForgeFix(Box::new(FixDone::Report {
                thread_id,
                sha,
                query,
                result,
            }))
        });
    }

    pub(super) fn fix_done(&mut self, done: FixDone) {
        match done {
            FixDone::Report {
                thread_id,
                sha,
                query,
                result,
            } => {
                let current = self
                    .threads
                    .get(&thread_id)
                    .filter(|t| !t.archived)
                    .and_then(|t| t.pr_status.as_ref())
                    .is_some_and(|s| s.head_sha == sha);
                let report = match result {
                    Ok(report) if current => report,
                    Ok(_) => {
                        // Pushed meanwhile: its own checks decide.
                        self.forge.fixing.remove(&thread_id);
                        match query {
                            Some(id) => self.emit(CoreEvent::Reply {
                                id,
                                result: Err("the branch moved on; check again".into()),
                            }),
                            None => self.undo_attempt(thread_id),
                        }
                        return;
                    }
                    Err(err) => {
                        self.forge.fixing.remove(&thread_id);
                        match query {
                            Some(id) => self.emit(CoreEvent::Reply {
                                id,
                                result: Err(err),
                            }),
                            None => {
                                self.undo_attempt(thread_id);
                                self.thread_notice(
                                    thread_id,
                                    &format!(
                                        "The failed checks could not be read, so no fix was \
                                         sent: {err}"
                                    ),
                                );
                            }
                        }
                        return;
                    }
                };
                // Only the fix turn's own work may be pushed for the user:
                // not while the thread or a folder sharer has a turn of its
                // own, nor when the folder held other work (found by the job).
                let hold = report.hold.or_else(|| {
                    let thread = self.threads.get(&thread_id)?;
                    let project = self.projects.get(&thread.project_id)?;
                    let cwd = thread.cwd(project);
                    let busy = self.is_busy(thread_id)
                        || self
                            .folder_sharers(thread_id, cwd)
                            .iter()
                            .any(|t| self.is_busy(t.id));
                    busy.then(|| "a turn is running in the thread's folder".to_owned())
                });
                let automatic = query.is_none() && hold.is_none();
                if query.is_none()
                    && let Some(why) = &hold
                {
                    self.thread_notice(
                        thread_id,
                        &format!(
                            "The failed checks went to the agent, but its fix will not be pushed \
                             for you: {why}. Push it from the PR tab once you have looked."
                        ),
                    );
                }
                let branch_name = self
                    .threads
                    .get(&thread_id)
                    .and_then(|t| t.pr.as_ref())
                    .map(|l| l.head_branch.clone())
                    .unwrap_or_default();
                let prompt =
                    blongo_forge::pr::fix_prompt(&branch_name, &sha, &report.checks, automatic);
                let run_id = RunId::new();
                self.forge.fixes.insert(
                    run_id,
                    Fix {
                        thread_id,
                        automatic,
                        names: report.checks.iter().map(|c| c.name.clone()).collect(),
                        query,
                        tainted: false,
                    },
                );
                let command = Command::MessageDispatch {
                    thread_id,
                    message_id: ItemId::new(),
                    run_id,
                    text: prompt,
                    delivery: Delivery::Queue,
                };
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    command,
                    Reply::Fix(run_id),
                )));
            }
            FixDone::Send {
                thread_id,
                query,
                key,
                result,
            } => {
                self.release_key(key);
                match result {
                    Ok(SendOutcome::Prompt(text)) if self.live_thread(thread_id).is_ok() => {
                        let command = Command::MessageDispatch {
                            thread_id,
                            message_id: ItemId::new(),
                            run_id: RunId::new(),
                            text,
                            delivery: Delivery::Queue,
                        };
                        self.deferred.push_back(Deferred::Dispatch(Pending::new(
                            command,
                            Reply::Query(query),
                        )));
                    }
                    Ok(SendOutcome::Prompt(_)) => self.emit(CoreEvent::Reply {
                        id: query,
                        result: Err("the thread is gone".into()),
                    }),
                    Ok(SendOutcome::Done(text)) => self.emit(CoreEvent::Reply {
                        id: query,
                        result: Ok(QueryReply::Done(text)),
                    }),
                    Err(err) => self.emit(CoreEvent::Reply {
                        id: query,
                        result: Err(err),
                    }),
                }
            }
            FixDone::Pushed { thread_id, result } => {
                self.release_key(Key::Thread(thread_id));
                let message = match &result {
                    Ok(text) => text.clone(),
                    Err(err) => format!("The CI fix was not pushed: {err}"),
                };
                if self.live_thread(thread_id).is_ok() {
                    self.thread_notice(thread_id, &message);
                }
                if result.is_ok() {
                    self.forge.hurry(thread_id, Duration::from_secs(5));
                }
            }
        }
    }

    /// `Query::PrComments`: send the unresolved review threads (or the
    /// chosen ones) to the agent.
    pub(super) fn pr_comments(&mut self, id: QueryId, thread_id: ThreadId, ids: Vec<String>) {
        let link = (|| {
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let thread = self.live_thread(thread_id)?;
            let link = thread
                .pr
                .clone()
                .ok_or_else(|| "no pull request is linked".to_owned())?;
            let cwd = self
                .projects
                .get(&thread.project_id)
                .map(|p| PathBuf::from(thread.cwd(p)));
            Ok((link, cwd))
        })();
        let (link, cwd) = match link {
            Ok(found) => found,
            Err(err) => {
                return self.emit(CoreEvent::Reply {
                    id,
                    result: Err(err),
                });
            }
        };
        let ctx = self.forge_ctx();
        self.spawn_job(Key::None, async move {
            let result = comments_report(&ctx, &link, cwd.as_deref(), &ids).await;
            JobDone::ForgeFix(Box::new(FixDone::Send {
                thread_id,
                query: id,
                key: Key::None,
                result,
            }))
        });
    }

    /// `Query::PrMergeBase` (a mutation holding `key`): merge the base
    /// branch in; conflicts go to the agent.
    pub(super) fn pr_merge_base(&mut self, id: QueryId, thread_id: ThreadId, key: Key) {
        let job = (|| {
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let (thread, cwd) = self.thread_cwd(thread_id)?;
            self.ensure_idle(&thread)?;
            if let Some(busy) = self
                .folder_sharers(thread.id, &cwd)
                .iter()
                .find(|t| self.is_busy(t.id))
            {
                return Err(format!(
                    "\"{}\" works in the same folder and is running; wait for it to finish",
                    busy.title
                ));
            }
            if self.forge.fixing.contains(&thread.id) {
                return Err("a CI fix is on its way; merge after it".into());
            }
            let link = thread.pr.clone().ok_or("no pull request is linked")?;
            let ours = thread.worktree.as_ref().map(|w| w.branch.as_str());
            if link.read_only || !forge::owns_branch(&thread) || ours != Some(&link.head_branch) {
                return Err(
                    "only the thread that owns the pull request's branch can merge into it".into(),
                );
            }
            Ok((PathBuf::from(cwd), link))
        })();
        match job {
            Ok((cwd, link)) => self.spawn_job(key, async move {
                let result = merge_base_in(&cwd, &link).await;
                JobDone::ForgeFix(Box::new(FixDone::Send {
                    thread_id,
                    query: id,
                    key,
                    result,
                }))
            }),
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    /// The fix message was refused (the thread went away, …).
    pub(super) fn fix_refused(&mut self, run_id: RunId, reason: &str) {
        if let Some(fix) = self.forge.fixes.remove(&run_id) {
            self.forge.fixing.remove(&fix.thread_id);
            match fix.query {
                Some(id) => self.emit(CoreEvent::Reply {
                    id,
                    result: Err(reason.to_owned()),
                }),
                None => eprintln!("blongo-core: CI fix not sent: {reason}"),
            }
        }
    }

    /// The fix message is in: answer the user who asked for it.
    pub(super) fn fix_sent(&mut self, run_id: RunId) {
        if let Some(id) = self.forge.fixes.get(&run_id).and_then(|f| f.query) {
            self.emit(CoreEvent::Reply {
                id,
                result: Ok(QueryReply::Done("sent to the agent".into())),
            });
        }
    }

    /// A run started: a fix still to end in the same folder no longer
    /// holds only its own work.
    pub(super) fn taint_fixes(&mut self, thread_id: ThreadId, run_id: RunId) {
        if self.forge.fixes.is_empty() {
            return;
        }
        let folder =
            |t: Option<&Thread>| t.and_then(|t| t.worktree.as_ref()).map(|w| w.path.clone());
        let here = folder(self.threads.get(&thread_id));
        for (fix_run, fix) in self.forge.fixes.iter_mut() {
            if *fix_run != run_id
                && (fix.thread_id == thread_id
                    || (here.is_some() && folder(self.threads.get(&fix.thread_id)) == here))
            {
                fix.tainted = true;
            }
        }
    }

    /// An automatic fix that did not happen gives its attempt back.
    fn undo_attempt(&mut self, thread_id: ThreadId) {
        let mut state = self.fix_state(thread_id);
        if state.attempts > 0 {
            state.attempts -= 1;
            state.sha.clear();
            self.set_fix_state(thread_id, &state);
        }
    }

    /// A run ended: an automatic fix's changes are committed and pushed.
    pub(super) fn finish_fix(&mut self, run_id: RunId, status: RunStatus) {
        let Some(fix) = self.forge.fixes.remove(&run_id) else {
            return;
        };
        self.forge.fixing.remove(&fix.thread_id);
        if !fix.automatic {
            return;
        }
        if status != RunStatus::Completed {
            self.thread_notice(
                fix.thread_id,
                &format!(
                    "The CI fix turn ended {}; nothing was pushed.",
                    format!("{status:?}").to_lowercase()
                ),
            );
            return;
        }
        let Some(thread) = self.threads.get(&fix.thread_id).filter(|t| !t.archived) else {
            return;
        };
        let (Some(link), Some(project)) =
            (thread.pr.clone(), self.projects.get(&thread.project_id))
        else {
            return;
        };
        let why = if fix.tainted {
            Some("another turn ran in the folder meanwhile")
        } else if !project.forge.auto_fix_ci {
            Some("automatic fixes were turned off")
        } else if !forge::owns_branch(thread) || link.read_only {
            Some("the thread no longer owns the branch")
        } else {
            None
        };
        if let Some(why) = why {
            self.thread_notice(
                fix.thread_id,
                &format!("The CI fix was not pushed: {why}. Push it from the PR tab once you have looked."),
            );
            return;
        }
        let cwd = PathBuf::from(thread.cwd(project));
        let thread_id = fix.thread_id;
        let message = format!("Fix CI: {}", fix.names.join(", "));
        self.spawn_job(Key::Thread(thread_id), async move {
            let result = push_fix(&cwd, &link.head_branch, &message).await;
            JobDone::ForgeFix(Box::new(FixDone::Pushed { thread_id, result }))
        });
    }

    /// A notice in the thread's timeline (outside any turn), committed
    /// by [`Self::flush_notices`]: this runs while events are applied,
    /// where a nested commit would emit events out of order.
    pub(super) fn thread_notice(&mut self, thread_id: ThreadId, message: &str) {
        self.forge.notices.push((thread_id, message.to_owned()));
    }

    /// Commit the notices queued by [`Self::thread_notice`].
    pub(super) fn flush_notices(&mut self) {
        for (thread_id, message) in std::mem::take(&mut self.forge.notices) {
            if self.live_thread(thread_id).is_ok() {
                self.commit_notice(thread_id, &message);
            }
        }
    }

    fn commit_notice(&mut self, thread_id: ThreadId, message: &str) {
        if let Ok(item) = self.new_item(
            thread_id,
            None,
            ItemKind::SystemNotice {
                message: message.to_owned(),
            },
            "",
        ) {
            self.commit_events(vec![EventKind::ItemAdded {
                item: Arc::new(item),
            }]);
        }
    }
}

/// What CI said of the failed checks, and whether the folder allows an
/// automatic fix to be pushed.
async fn fix_report(
    ctx: &ForgeCtx,
    link: &PrLink,
    cwd: &std::path::Path,
    sha: &str,
    automatic: bool,
) -> Result<Report, String> {
    // The fix is made where the branch is checked out.
    if remote::current_branch(cwd).await.as_deref() != Some(link.head_branch.as_str()) {
        return Err(format!(
            "the thread's folder does not have {} checked out",
            link.head_branch
        ));
    }
    let repo = RepoRef::parse(&format!("https://{}/{}", link.host, link.repo))
        .ok_or("not a GitHub repository")?;
    let gh = ctx.client(&repo, ctx.token(&repo.host).await?);
    let checks: Vec<FailedCheck> = gh
        .failure_report(&repo, sha)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    // An automatic fix is pushed for the user, so the folder must hold
    // nothing but the failed commit.
    let hold = if !automatic {
        None
    } else if branch::unmerged(cwd).await {
        Some("a merge is in progress in the thread's folder".to_owned())
    } else if !branch::uncommitted_files(cwd).await.is_empty() {
        Some("the thread's folder has uncommitted changes".to_owned())
    } else if branch::head(cwd).await.as_deref() != Some(sha) {
        Some("the thread's folder is not at the failed commit".to_owned())
    } else {
        None
    };
    Ok(Report { checks, hold })
}

/// The unresolved review threads as the message for the agent.
async fn comments_report(
    ctx: &ForgeCtx,
    link: &PrLink,
    cwd: Option<&std::path::Path>,
    ids: &[String],
) -> Result<SendOutcome, String> {
    let repo = RepoRef::parse(&format!("https://{}/{}", link.host, link.repo))
        .ok_or("not a GitHub repository")?;
    let gh = ctx.client(&repo, ctx.token(&repo.host).await?);
    let detail = gh
        .detail(&repo, link.number)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    let threads: Vec<_> = detail
        .threads
        .into_iter()
        .filter(|t| !t.resolved && (ids.is_empty() || ids.contains(&t.id)))
        .collect();
    if threads.is_empty() {
        return Err("no unresolved review comments".into());
    }
    // The commented line as the folder has it, when the branch is
    // checked out there and the comment is not outdated.
    let checked_out = match cwd {
        Some(cwd) => remote::current_branch(cwd).await.as_deref() == Some(&link.head_branch),
        None => false,
    };
    let quotes: Vec<Option<String>> = threads
        .iter()
        .map(|t| {
            let (cwd, line) = (cwd.filter(|_| checked_out)?, t.line?);
            if t.outdated {
                return None;
            }
            code_line(cwd, &t.path, line)
        })
        .collect();
    Ok(SendOutcome::Prompt(blongo_forge::pr::comments_prompt(
        &threads, &quotes,
    )))
}

/// Line `line` (from 1) of the file at `path` inside `cwd`: a relative
/// path of plain components, a file of at most 1 MiB.
fn code_line(cwd: &std::path::Path, path: &str, line: u32) -> Option<String> {
    use std::path::Component;
    let rel = std::path::Path::new(path);
    if path.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let full = cwd.join(rel);
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(full).ok()?;
    let code = text.lines().nth(line.checked_sub(1)? as usize)?;
    let code: String = code.chars().take(300).collect();
    (!code.trim().is_empty()).then_some(code)
}

/// Fetch the base branch and merge it in; conflicts become the message
/// for the agent.
async fn merge_base_in(cwd: &std::path::Path, link: &PrLink) -> Result<SendOutcome, String> {
    let name = &link.head_branch;
    if remote::current_branch(cwd).await.as_deref() != Some(name.as_str()) {
        return Err(format!(
            "the thread's folder does not have {name} checked out"
        ));
    }
    let remote_name = remote::remote_name(cwd)
        .await
        .ok_or("this folder has no git remote")?;
    let base = &link.base_branch;
    branch::fetch(cwd, &remote_name, base, branch::FETCH_TIMEOUT).await?;
    Ok(match branch::merge_base(cwd, &remote_name, base).await? {
        branch::MergeOutcome::UpToDate => {
            SendOutcome::Done(format!("{name} has everything from {base} already."))
        }
        branch::MergeOutcome::Merged => SendOutcome::Done(format!(
            "Merged {remote_name}/{base} into {name}; push to update the pull request."
        )),
        branch::MergeOutcome::Conflicts(files) => SendOutcome::Prompt(
            blongo_forge::pr::conflicts_prompt(name, &format!("{remote_name}/{base}"), &files),
        ),
    })
}

/// Commit what the fix turn left uncommitted and push the branch if it
/// has commits the remote lacks (never forced).
async fn push_fix(
    cwd: &std::path::Path,
    branch_name: &str,
    message: &str,
) -> Result<String, String> {
    if remote::current_branch(cwd).await.as_deref() != Some(branch_name) {
        return Err(format!(
            "the folder no longer has {branch_name} checked out"
        ));
    }
    if branch::unmerged(cwd).await {
        return Err("a merge is in progress or files are in conflict".into());
    }
    let mut committed = false;
    if !branch::uncommitted_files(cwd).await.is_empty() {
        let message: String = message.chars().take(200).collect();
        blongo_git::workspace::commit_all(cwd, &message)
            .await
            .map_err(|e| format!("committing failed: {e:#}"))?;
        committed = true;
    }
    let ahead = remote::ahead_behind(cwd, branch_name).await.map(|(a, _)| a);
    if ahead == Some(0) && !committed {
        return Ok("The CI fix turn changed nothing, so nothing was pushed.".into());
    }
    let remote_name = remote::remote_name(cwd)
        .await
        .ok_or("this folder has no git remote")?;
    branch::push(cwd, &remote_name, branch_name)
        .await
        .map_err(|e| format!("pushing failed: {e}"))?;
    Ok(format!(
        "Pushed the CI fix to {branch_name}; the checks run again."
    ))
}
