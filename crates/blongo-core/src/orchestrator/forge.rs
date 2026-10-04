//! GitHub pull requests: linking a thread to its branch's pull request and
//! polling linked ones.
//!
//! No resident process and no HTTP for threads that have nothing on
//! GitHub: a thread is looked up only once its branch was pushed (checked
//! locally), and linked pull requests are polled on the core's timer, one
//! job at a time, one GraphQL request per repository. Polls come fast for
//! a few minutes after something happened (a link, a turn, a refresh),
//! every minute while checks run, every five minutes otherwise, back off
//! on errors and stop once the pull request is merged or closed.

use std::path::PathBuf;

use blongo_forge::github::{GhError, GitHub, MAX_BATCH, NewPull, PullInfo, RateLimit, RepoInfo};
use blongo_forge::remote::{self, RepoRef};
use blongo_forge::{auth, branch};
use blongo_protocol::{
    BaseBranch, ChecksState, Delivery, ForgeSettings, PrCreateRequest, PrLink, PrPrepare, PrStatus,
    branch_slug, parse_pr_ref,
};

use super::*;

const FAST: Duration = Duration::from_secs(15);
const FAST_WINDOW: Duration = Duration::from_secs(3 * 60);
const CHECKS_RUNNING: Duration = Duration::from_secs(60);
const IDLE: Duration = Duration::from_secs(5 * 60);
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// After a turn ends: give the agent's push a moment to reach GitHub.
const AFTER_TURN: Duration = Duration::from_secs(5);

/// Polling state (in memory; rebuilt from the threads at start).
#[derive(Default)]
pub(super) struct ForgeRt {
    /// A poll job is running.
    running: bool,
    /// When each linked thread is due.
    due: HashMap<ThreadId, Instant>,
    /// Fast polling until then.
    fast_until: HashMap<ThreadId, Instant>,
    /// Consecutive failed polls.
    failures: HashMap<ThreadId, u32>,
    /// Threads whose branch's pull request to look for at the next tick.
    lookup: HashSet<ThreadId>,
    /// GitHub said the rate limit is low: poll half as often.
    low_rate: bool,
    /// `PrRefresh` queries waiting for their thread's next check.
    waiters: HashMap<ThreadId, Vec<QueryId>>,
    /// `PrDraft` queries waiting for the run that answers them.
    pub drafts: HashMap<RunId, QueryId>,
    /// CI fix messages waiting for their run to end.
    pub fixes: HashMap<RunId, super::autofix::Fix>,
    /// Threads with a CI fix on its way (report, queued or running turn).
    pub fixing: HashSet<ThreadId>,
    /// The head commit and checks state last committed per thread (a
    /// failure is acted on when it is news).
    pub seen: HashMap<ThreadId, (String, ChecksState)>,
    /// Thread notices to commit once the current commit is done.
    pub notices: Vec<(ThreadId, String)>,
    /// The pull request state last committed per thread (a merge is
    /// acted on when it is news).
    pub states: HashMap<ThreadId, blongo_protocol::PrState>,
    /// What a query answers once its command committed (default: "sent
    /// to the agent").
    pub answers: HashMap<QueryId, String>,
    /// Threads being archived because their pull request was merged (said
    /// once the archive is committed).
    pub auto_archiving: HashSet<ThreadId>,
    tokens: TokenCache,
}

/// How long a token is reused before it is read again (saves running
/// `gh auth token` on every fast poll).
const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);

type TokenCache = Arc<std::sync::Mutex<HashMap<String, (Instant, String)>>>;

/// What a poll job needs, resolved on the loop.
#[derive(Clone)]
pub(super) struct ForgeCtx {
    tokens: PathBuf,
    gh: Option<PathBuf>,
    api: Option<String>,
    cache: TokenCache,
}

impl ForgeCtx {
    pub(super) fn client(&self, repo: &RepoRef, token: String) -> GitHub {
        match &self.api {
            Some(api) => GitHub::with_api(api, token),
            None => GitHub::new(repo, token),
        }
    }

    pub(super) async fn token(&self, host: &str) -> Result<String, String> {
        if let Some((at, token)) = self.cache.lock().unwrap().get(host)
            && at.elapsed() < TOKEN_TTL
        {
            return Ok(token.clone());
        }
        let token = auth::token_for(host, &self.tokens, self.gh.as_deref()).await;
        let mut cache = self.cache.lock().unwrap();
        match &token {
            Some(t) => cache.insert(host.to_owned(), (Instant::now(), t.token.clone())),
            None => cache.remove(host),
        };
        token.map(|t| t.token).ok_or_else(|| {
            format!(
                "no GitHub token for {host}: sign in with `gh auth login` or add a token in \
                 Settings"
            )
        })
    }

    /// The message for a failed call; a refused token is read again next
    /// time.
    pub(super) fn fail(&self, host: &str, err: GhError) -> String {
        if matches!(err, GhError::Auth(_)) {
            self.cache.lock().unwrap().remove(host);
        }
        describe(err)
    }
}

/// A linked pull request, found or checked.
pub(super) struct Found {
    pub link: PrLink,
    pub status: PrStatus,
    pub info: RepoInfo,
}

struct PollTarget {
    thread_id: ThreadId,
    link: PrLink,
}

struct LookupTarget {
    thread_id: ThreadId,
    cwd: PathBuf,
    /// The worktree's branch (`None`: whatever the folder has checked out).
    branch: Option<String>,
    /// Last known default branch of the repository, if cached.
    cached_info: Option<RepoInfo>,
}

/// A PR tab query or change ended.
pub(super) struct QueryDone {
    id: QueryId,
    thread_id: ThreadId,
    /// Held while it ran.
    key: Option<Key>,
    /// The linked pull request it was about (its status is kept only
    /// while the thread still links it).
    link: Option<PrLink>,
    /// What GitHub said of the pull request (detail).
    status: Option<PrStatus>,
    /// The pull request changed (edit, push): poll it now.
    changed: bool,
    /// The thread's branch got this name (create).
    renamed: Option<String>,
    /// The pull request created (or found open) for the branch.
    created: Option<Box<Found>>,
    /// The repository as GitHub described it (prepare).
    info: Option<(String, String, RepoInfo)>,
    result: Result<QueryReply, String>,
}

impl QueryDone {
    fn new(id: QueryId, thread_id: ThreadId, result: Result<QueryReply, String>) -> Self {
        Self {
            id,
            thread_id,
            key: None,
            link: None,
            status: None,
            changed: false,
            renamed: None,
            created: None,
            info: None,
            result,
        }
    }
}

pub(super) struct ForgeDone {
    polled: Vec<(ThreadId, PrLink, Result<PrStatus, String>)>,
    found: Vec<(ThreadId, Result<Option<Found>, String>)>,
    rate: Option<RateLimit>,
}

impl ForgeRt {
    fn interval(&self, thread_id: ThreadId, status: &PrStatus, now: Instant) -> Duration {
        let base = if self.fast_until.get(&thread_id).is_some_and(|t| *t > now) {
            FAST
        } else if status.checks.state == ChecksState::Pending {
            CHECKS_RUNNING
        } else {
            IDLE
        };
        if self.low_rate { base * 2 } else { base }
    }

    fn backoff(failures: u32) -> Duration {
        let secs = FAST.as_secs().saturating_mul(1 << failures.min(10));
        Duration::from_secs(secs).min(MAX_BACKOFF)
    }

    fn forget(&mut self, thread_id: ThreadId) {
        self.due.remove(&thread_id);
        self.fast_until.remove(&thread_id);
        self.failures.remove(&thread_id);
        self.lookup.remove(&thread_id);
        // What was seen of the previous pull request is no news base for
        // the next one.
        self.seen.remove(&thread_id);
        self.states.remove(&thread_id);
    }

    /// Something happened on the thread: poll it soon and fast for a while.
    pub(super) fn hurry(&mut self, thread_id: ThreadId, after: Duration) {
        let now = Instant::now();
        self.fast_until.insert(thread_id, now + FAST_WINDOW);
        self.failures.remove(&thread_id);
        self.due_at(thread_id, now + after);
    }

    /// Due at `at`, unless something (a refresh, a turn while the job ran)
    /// already made it due sooner.
    fn due_at(&mut self, thread_id: ThreadId, at: Instant) {
        self.due
            .entry(thread_id)
            .and_modify(|d| *d = (*d).min(at))
            .or_insert(at);
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        if self.running {
            return None;
        }
        if !self.lookup.is_empty() {
            return Some(Instant::now());
        }
        self.due.values().min().copied()
    }
}

impl Orchestrator {
    pub(super) fn forge_ctx(&self) -> ForgeCtx {
        ForgeCtx {
            tokens: self.config.forge_tokens.clone(),
            gh: self.config.gh_program.clone(),
            api: self.config.github_api.clone(),
            cache: self.forge.tokens.clone(),
        }
    }

    pub(super) fn cached_repo_info(&self, host: &str, repo: &str) -> Option<RepoInfo> {
        let (json, _) = self
            .store
            .forge_cache(&format!("repo:{host}/{repo}"))
            .ok()
            .flatten()?;
        serde_json::from_str(&json).ok()
    }

    fn cache_repo_info(&self, link: &PrLink, info: &RepoInfo) {
        self.cache_info(&link.host, &link.repo, info);
    }

    fn cache_info(&self, host: &str, repo: &str, info: &RepoInfo) {
        if let Ok(json) = serde_json::to_string(info) {
            let key = format!("repo:{host}/{repo}");
            if let Err(err) = self.store.set_forge_cache(&key, &json) {
                eprintln!("blongo-core: forge cache: {err:#}");
            }
        }
    }

    /// Where a project's new worktrees start (`None`: integration off, its
    /// HEAD as before).
    pub(super) fn base_plan(&self, project: &Project) -> Option<BasePlan> {
        if !self.config.forge {
            return None;
        }
        Some(BasePlan {
            ctx: self.forge_ctx(),
            custom: match &project.forge.base_branch {
                BaseBranch::Custom { name } => Some(name.clone()),
                BaseBranch::GithubDefault => None,
            },
            cached: self
                .store
                .forge_cache(&format!("base:{}", project.id))
                .ok()
                .flatten()
                .map(|(name, _)| name),
        })
    }

    /// Remember the base branch a project's worktree started from.
    pub(super) fn cache_base(&self, project_id: ProjectId, base: &str) {
        if let Err(err) = self
            .store
            .set_forge_cache(&format!("base:{project_id}"), base)
        {
            eprintln!("blongo-core: forge cache: {err:#}");
        }
    }

    /// Schedule every linked thread and look up pushed worktree branches
    /// (called once at start).
    pub(super) fn forge_start(&mut self) {
        if !self.config.forge {
            return;
        }
        let now = Instant::now();
        for (i, thread) in self.threads.values().filter(|t| !t.archived).enumerate() {
            if let Some(status) = &thread.pr_status {
                self.forge
                    .seen
                    .insert(thread.id, (status.head_sha.clone(), status.checks.state));
                self.forge.states.insert(thread.id, status.state);
            }
            match (&thread.pr, &thread.pr_status) {
                (Some(_), Some(status)) if status.state.is_final() => {}
                (Some(_), _) => {
                    // Spread the first polls over a few seconds.
                    let at = now + Duration::from_millis(500 * (i as u64 % 20));
                    self.forge.due.insert(thread.id, at);
                }
                (None, _) if owns_branch(thread) => {
                    self.forge.lookup.insert(thread.id);
                }
                (None, _) => {}
            }
        }
    }

    /// Keep polling state in step with committed events.
    pub(super) fn forge_track(&mut self, event: &EventKind) {
        match event {
            EventKind::ThreadPrLinked { thread_id, pr, .. } => {
                self.forge.forget(*thread_id);
                if pr.is_some() && self.config.forge {
                    self.forge.hurry(*thread_id, FAST);
                } else if pr.is_none() {
                    self.answer_pr_waiters(*thread_id, Ok("unlinked".into()));
                }
            }
            EventKind::RunStatusChanged { run_id, status, .. } if status.is_terminal() => {
                self.answer_draft(*run_id, *status);
                self.finish_fix(*run_id, *status);
            }
            EventKind::RunStatusChanged {
                thread_id,
                run_id,
                status: RunStatus::Running,
                ..
            } => self.taint_fixes(*thread_id, *run_id),
            EventKind::ThreadPrStatus {
                thread_id,
                status: Some(status),
            } => {
                self.consider_fix(*thread_id, status);
                self.consider_merged(*thread_id, status);
            }
            EventKind::ThreadArchived { thread_id } => {
                if self.forge.auto_archiving.remove(thread_id)
                    && let Some(thread) = self.threads.get(thread_id)
                {
                    let label = thread.pr.as_ref().map(|l| l.label()).unwrap_or_default();
                    self.emit(CoreEvent::Notice {
                        message: format!(
                            "{label} was merged, so \"{}\" was archived.",
                            thread.title
                        ),
                    });
                }
                self.forge.forget(*thread_id);
                self.answer_pr_waiters(*thread_id, Err("the thread was archived".into()));
            }
            _ => {}
        }
    }

    /// A root run ended: the agent may have pushed.
    pub(super) fn forge_after_turn(&mut self, thread_id: ThreadId) {
        if !self.config.forge {
            return;
        }
        let Some(thread) = self.threads.get(&thread_id) else {
            return;
        };
        match &thread.pr {
            Some(_)
                if !thread
                    .pr_status
                    .as_ref()
                    .is_some_and(|s| s.state.is_final()) =>
            {
                self.forge.hurry(thread_id, AFTER_TURN);
            }
            Some(_) => {}
            None if owns_branch(thread) => {
                self.forge.lookup.insert(thread_id);
            }
            None => {}
        }
    }

    /// `Query::PrRefresh`: check now; answered when the check ends.
    pub(super) fn pr_refresh(&mut self, id: QueryId, thread_id: ThreadId) {
        if !self.config.forge {
            return self.emit(CoreEvent::Reply {
                id,
                result: Err("GitHub integration is turned off".into()),
            });
        }
        let Ok(thread) = self.live_thread(thread_id) else {
            return self.emit(CoreEvent::Reply {
                id,
                result: Err("unknown thread".into()),
            });
        };
        if thread.pr.is_some() {
            self.forge.hurry(thread_id, Duration::ZERO);
        } else {
            self.forge.lookup.insert(thread_id);
        }
        self.forge.waiters.entry(thread_id).or_default().push(id);
    }

    fn answer_pr_waiters(&mut self, thread_id: ThreadId, result: Result<String, String>) {
        for id in self.forge.waiters.remove(&thread_id).unwrap_or_default() {
            self.emit(CoreEvent::Reply {
                id,
                result: result.clone().map(QueryReply::Done),
            });
        }
    }

    /// Timer: start a poll job for what is due.
    pub(super) fn forge_tick(&mut self) {
        if !self.config.forge || self.forge.running {
            return;
        }
        let now = Instant::now();
        let due: Vec<ThreadId> = self
            .forge
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(t, _)| *t)
            .collect();
        let mut polls = Vec::new();
        let mut skipped = Vec::new();
        for thread_id in due {
            self.forge.due.remove(&thread_id);
            let link = self
                .threads
                .get(&thread_id)
                .filter(|t| !t.archived)
                .and_then(|t| t.pr.clone());
            match link {
                Some(link) => polls.push(PollTarget { thread_id, link }),
                None => skipped.push(thread_id),
            }
        }
        let mut lookups = Vec::new();
        for thread_id in std::mem::take(&mut self.forge.lookup) {
            let thread = self.threads.get(&thread_id).filter(|t| !t.archived);
            let project = thread.and_then(|t| self.projects.get(&t.project_id));
            let (Some(thread), Some(project)) = (thread, project) else {
                skipped.push(thread_id);
                continue;
            };
            if thread.pr.is_some() {
                // Linked meanwhile: a poll answers its waiters.
                self.forge.hurry(thread_id, Duration::ZERO);
                continue;
            }
            lookups.push(LookupTarget {
                thread_id,
                cwd: PathBuf::from(thread.cwd(project)),
                branch: thread.worktree.as_ref().map(|w| w.branch.clone()),
                cached_info: None,
            });
        }
        // Nothing will check these: never leave a refresh unanswered.
        for thread_id in skipped {
            if !polls.iter().any(|p| p.thread_id == thread_id) {
                self.answer_pr_waiters(thread_id, Err("nothing to check".into()));
            }
        }
        if polls.is_empty() && lookups.is_empty() {
            return;
        }
        for target in &mut lookups {
            // The repository is known only once the remote was read, which
            // happens in the job; the repository of the project's other
            // linked threads is a good guess for its default branch.
            let project_id = self.threads.get(&target.thread_id).map(|t| t.project_id);
            target.cached_info = self
                .threads
                .values()
                .filter(|t| t.id != target.thread_id && Some(t.project_id) == project_id)
                .filter_map(|t| t.pr.as_ref())
                .find_map(|l| self.cached_repo_info(&l.host, &l.repo));
        }
        self.forge.running = true;
        let ctx = self.forge_ctx();
        self.spawn_job(Key::None, async move {
            JobDone::Forge(Box::new(run_poll(ctx, polls, lookups).await))
        });
    }

    pub(super) fn forge_done(&mut self, done: ForgeDone) {
        self.forge.running = false;
        if let Some(rate) = done.rate {
            self.forge.low_rate = rate.is_low();
        }
        let now = Instant::now();
        let mut events = Vec::new();
        let mut answers = Vec::new();
        for (thread_id, link, result) in done.polled {
            let Some(thread) = self.threads.get(&thread_id) else {
                continue;
            };
            // Unlinked or relinked meanwhile: this result is stale.
            if thread.archived || thread.pr.as_ref() != Some(&link) {
                continue;
            }
            match result {
                Ok(status) => {
                    self.forge.failures.remove(&thread_id);
                    if !status.state.is_final() {
                        let next = now + self.forge.interval(thread_id, &status, now);
                        self.forge.due_at(thread_id, next);
                    }
                    if thread.pr_status.as_ref() != Some(&status) {
                        events.push(EventKind::ThreadPrStatus {
                            thread_id,
                            status: Some(status),
                        });
                    }
                    answers.push((thread_id, Ok(format!("{} checked", link.label()))));
                }
                Err(err) => {
                    let failures = self.forge.failures.entry(thread_id).or_insert(0);
                    *failures += 1;
                    let wait = ForgeRt::backoff(*failures);
                    self.forge.due_at(thread_id, now + wait);
                    let mut status = thread.pr_status.clone().unwrap_or_default();
                    if status.error.as_deref() != Some(err.as_str()) {
                        status.error = Some(err.clone());
                        events.push(EventKind::ThreadPrStatus {
                            thread_id,
                            status: Some(status),
                        });
                    }
                    answers.push((thread_id, Err(err)));
                }
            }
        }
        for (thread_id, result) in done.found {
            let Some(thread) = self.threads.get(&thread_id) else {
                continue;
            };
            match result {
                Ok(Some(found))
                    if thread.pr.is_none() && !thread.pr_dismissed && !thread.archived =>
                {
                    self.cache_repo_info(&found.link, &found.info);
                    let label = found.link.label();
                    events.push(EventKind::ThreadPrLinked {
                        thread_id,
                        pr: Some(found.link),
                        manual: false,
                    });
                    events.push(EventKind::ThreadPrStatus {
                        thread_id,
                        status: Some(found.status),
                    });
                    answers.push((thread_id, Ok(format!("linked {label}"))));
                }
                Ok(Some(_)) if thread.pr.is_some() => {
                    // Linked by hand meanwhile: its poll answers.
                    self.forge.hurry(thread_id, Duration::ZERO);
                }
                Ok(Some(_)) => answers.push((thread_id, Ok("automatic linking is off".into()))),
                Ok(None) => answers.push((thread_id, Ok("no pull request for this branch".into()))),
                Err(err) => answers.push((thread_id, Err(err))),
            }
        }
        self.commit_events(events);
        for (thread_id, result) in answers {
            self.answer_pr_waiters(thread_id, result);
        }
    }

    /// `Query::PrDetail` / `Query::PrEdit` / `Query::PrPrepare`: ask
    /// GitHub in a job, answer from there.
    pub(super) fn pr_query(&mut self, id: QueryId, query: Query) {
        let job = (|| {
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let thread = self.live_thread(query.thread_id())?;
            let project = self
                .projects
                .get(&thread.project_id)
                .ok_or("unknown project")?;
            let cwd = PathBuf::from(thread.cwd(project));
            if let Query::PrPrepare { .. } = query {
                let branch = own_branch(thread)?;
                let base = match &project.forge.base_branch {
                    BaseBranch::Custom { name } => Some(name.clone()),
                    BaseBranch::GithubDefault => None,
                };
                let title = (thread.title != DEFAULT_TITLE).then(|| thread.title.clone());
                return Ok((
                    thread.id,
                    None,
                    PrOp::Prepare {
                        cwd,
                        branch,
                        prefix: project.forge.branch_prefix.clone(),
                        base,
                        title,
                    },
                ));
            }
            let link = thread.pr.clone().ok_or("no pull request is linked")?;
            let op = match query {
                Query::PrDetail { .. } => PrOp::Detail {
                    cwd,
                    auto_fix: self.auto_fix_info(thread),
                    merge: self.merge_facts(&link),
                },
                Query::PrEdit { title, body, .. } => {
                    if link.read_only {
                        return Err(format!("{} is read-only here", link.label()));
                    }
                    let title = title.map(|t| t.trim().replace(['\n', '\r'], " "));
                    if title
                        .as_deref()
                        .is_some_and(|t| t.is_empty() || t.chars().count() > 256)
                    {
                        return Err("a title has 1 to 256 characters".into());
                    }
                    if body.as_deref().is_some_and(|b| b.chars().count() > 65_536) {
                        return Err("the description is longer than GitHub allows".into());
                    }
                    if title.is_none() && body.is_none() {
                        return Err("nothing to change".into());
                    }
                    PrOp::Edit { title, body }
                }
                _ => return Err("not a pull request query".into()),
            };
            Ok((thread.id, Some(link), op))
        })();
        match job {
            Ok((thread_id, link, op)) => self.spawn_pr_op(id, thread_id, Key::None, link, op),
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    /// `Query::PrCreate` / `Query::PrPush`: change the branch and GitHub
    /// while holding `key` (the thread and, for a create, every thread).
    pub(super) fn pr_mutate(&mut self, id: QueryId, query: Query, key: Key) {
        let job = (|| {
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let (thread, cwd) = self.thread_cwd(query.thread_id())?;
            self.ensure_idle(&thread)?;
            let branch = own_branch(&thread)?;
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
            let cwd = PathBuf::from(cwd);
            let op = match query {
                Query::PrCreate { request, .. } => {
                    if let Some(pr) = &thread.pr
                        && !thread
                            .pr_status
                            .as_ref()
                            .is_some_and(|s| s.state.is_final())
                    {
                        return Err(format!("{} is linked already", pr.label()));
                    }
                    PrOp::Create {
                        cwd,
                        branch,
                        request: checked_request(request)?,
                    }
                }
                Query::PrPush { .. } => {
                    if thread.pr.as_ref().is_some_and(|p| p.head_branch != branch) {
                        return Err("the linked pull request is for another branch".into());
                    }
                    PrOp::Push { cwd, branch }
                }
                _ => return Err("not a pull request change".into()),
            };
            Ok((thread.id, thread.pr.clone(), op))
        })();
        match job {
            Ok((thread_id, link, op)) => self.spawn_pr_op(id, thread_id, key, link, op),
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    fn spawn_pr_op(
        &mut self,
        id: QueryId,
        thread_id: ThreadId,
        key: Key,
        link: Option<PrLink>,
        op: PrOp,
    ) {
        let ctx = self.forge_ctx();
        self.spawn_job(key, async move {
            let mut done = run_pr_op(&ctx, link.as_ref(), op, id, thread_id).await;
            done.key = Some(key);
            done.link = link;
            JobDone::ForgeQuery(Box::new(done))
        });
    }

    /// `Query::PrDraft`: send the prompt as a message (queued behind a
    /// running turn); answered when that turn ends.
    pub(super) fn pr_draft(&mut self, id: QueryId, thread_id: ThreadId, prompt: String) {
        if prompt.trim().is_empty() {
            return self.emit(CoreEvent::Reply {
                id,
                result: Err("nothing to ask".into()),
            });
        }
        let run_id = RunId::new();
        self.forge.drafts.insert(run_id, id);
        let command = Command::MessageDispatch {
            thread_id,
            message_id: ItemId::new(),
            run_id,
            text: prompt,
            delivery: Delivery::Queue,
        };
        self.deferred
            .push_back(Deferred::Dispatch(Pending::new(command, Reply::Query(id))));
    }

    /// A run ended: answer the draft query waiting for it.
    fn answer_draft(&mut self, run_id: RunId, status: RunStatus) {
        let Some(id) = self.forge.drafts.remove(&run_id) else {
            return;
        };
        let result = if status == RunStatus::Completed {
            let answer = self
                .store
                .run_items(run_id)
                .unwrap_or_default()
                .into_iter()
                .rev()
                .find(|i| matches!(i.kind, ItemKind::AssistantMessage { .. }))
                .map(|i| i.text.to_string())
                .unwrap_or_default();
            blongo_forge::pr::parse_draft(&answer)
                .map(QueryReply::PrDraft)
                .ok_or_else(|| "the agent's answer held no draft (a JSON title and body)".into())
        } else {
            Err(format!(
                "the agent did not answer (the turn ended {})",
                format!("{status:?}").to_lowercase()
            ))
        };
        self.emit(CoreEvent::Reply { id, result });
    }

    pub(super) fn forge_query_done(&mut self, done: QueryDone) {
        if let Some(key) = done.key {
            self.release_key(key);
        }
        let mut events = Vec::new();
        if let Some(branch) = &done.renamed {
            // Everything working in the worktree is on the renamed branch.
            let path = self
                .threads
                .get(&done.thread_id)
                .and_then(|t| t.worktree.as_ref())
                .map(|w| w.path.clone());
            for t in self.threads.values() {
                if path.is_some() && t.worktree.as_ref().map(|w| &w.path) == path.as_ref() {
                    events.push(EventKind::ThreadBranchRenamed {
                        thread_id: t.id,
                        branch: branch.clone(),
                    });
                }
            }
        }
        if let Some((host, repo, info)) = &done.info {
            self.cache_info(host, repo, info);
        }
        let live = self
            .threads
            .get(&done.thread_id)
            .is_some_and(|t| !t.archived);
        if let Some(found) = done.created
            && live
        {
            self.cache_repo_info(&found.link, &found.info);
            events.push(EventKind::ThreadPrLinked {
                thread_id: done.thread_id,
                pr: Some(found.link),
                manual: true,
            });
            events.push(EventKind::ThreadPrStatus {
                thread_id: done.thread_id,
                status: Some(found.status),
            });
        }
        let current = self
            .threads
            .get(&done.thread_id)
            .filter(|t| !t.archived && done.link.is_some() && t.pr == done.link);
        if let Some(thread) = current {
            if let Some(status) = done.status
                && thread.pr_status.as_ref() != Some(&status)
            {
                events.push(EventKind::ThreadPrStatus {
                    thread_id: done.thread_id,
                    status: Some(status),
                });
            }
            if done.changed {
                self.forge.hurry(done.thread_id, Duration::ZERO);
            }
        }
        if !events.is_empty() {
            self.commit_events(events);
        }
        self.emit(CoreEvent::Reply {
            id: done.id,
            result: done.result,
        });
    }

    // ------------------------------------------------------------- commands

    /// `thread.link_pr`: what to check before deciding.
    pub(super) fn prepare_link(
        &self,
        thread_id: ThreadId,
        input: &str,
    ) -> Result<PrepPlan, String> {
        if !self.config.forge {
            return Err("GitHub integration is turned off".into());
        }
        let thread = self.live_thread(thread_id)?;
        let project = self
            .projects
            .get(&thread.project_id)
            .ok_or("unknown project")?;
        let reference =
            parse_pr_ref(input).ok_or("enter a pull request URL, owner/name#number or #number")?;
        Ok(PrepPlan::LinkPr {
            ctx: self.forge_ctx(),
            cwd: PathBuf::from(thread.cwd(project)),
            reference,
        })
    }

    pub(super) fn decide_link(
        &mut self,
        batch: &mut Batch,
        thread_id: ThreadId,
        found: Option<Box<Found>>,
    ) -> Result<(), String> {
        self.live_thread(thread_id)?;
        let found = *found.ok_or("the pull request could not be checked")?;
        self.cache_repo_info(&found.link, &found.info);
        batch.events.push(EventKind::ThreadPrLinked {
            thread_id,
            pr: Some(found.link),
            manual: true,
        });
        batch.events.push(EventKind::ThreadPrStatus {
            thread_id,
            status: Some(found.status),
        });
        Ok(())
    }

    pub(super) fn decide_unlink(
        &mut self,
        batch: &mut Batch,
        thread_id: ThreadId,
    ) -> Result<(), String> {
        let thread = self.live_thread(thread_id)?;
        if thread.pr.is_none() && thread.pr_dismissed {
            return Err("no pull request is linked".into());
        }
        batch.events.push(EventKind::ThreadPrLinked {
            thread_id,
            pr: None,
            manual: true,
        });
        Ok(())
    }

    pub(super) fn decide_set_forge(
        &mut self,
        batch: &mut Batch,
        project_id: ProjectId,
        settings: &ForgeSettings,
    ) -> Result<(), String> {
        if !self.projects.contains_key(&project_id) {
            return Err("unknown project".into());
        }
        let mut settings = settings.clone();
        settings.branch_prefix = settings.branch_prefix.trim().to_owned();
        if !settings.branch_prefix.is_empty() && !branch_name_ok(&settings.branch_prefix, true) {
            return Err(format!(
                "\"{}\" cannot start a branch name",
                settings.branch_prefix
            ));
        }
        if let BaseBranch::Custom { name } = &mut settings.base_branch {
            *name = name.trim().to_owned();
            if !branch_name_ok(name, false) {
                return Err(format!("\"{name}\" is not a branch name"));
            }
        }
        if !(2..=10).contains(&settings.auto_fix_max) {
            return Err("automatic CI fixes stop after 2 to 10 failing runs in a row".into());
        }
        batch.events.push(EventKind::ProjectForgeChanged {
            project_id,
            settings,
        });
        Ok(())
    }
}

/// Whether a thread's branch is its own to look a pull request up for:
/// subagent threads and forks share their source's worktree, and the
/// pull request belongs to the thread that made it.
pub(super) fn owns_branch(thread: &Thread) -> bool {
    thread.worktree.is_some()
        && !thread.pr_dismissed
        && thread.parent_thread_id.is_none()
        && thread.forked_from.is_none()
}

/// The branch a pull request is created from: the thread's own worktree
/// branch.
fn own_branch(thread: &Thread) -> Result<String, String> {
    if thread.parent_thread_id.is_some() || thread.forked_from.is_some() {
        return Err("create the pull request from the thread that started this branch".into());
    }
    thread
        .worktree
        .as_ref()
        .map(|w| w.branch.clone())
        .ok_or_else(|| "pull requests are created from threads with their own worktree".into())
}

/// The Create PR form, trimmed and checked.
fn checked_request(mut r: PrCreateRequest) -> Result<PrCreateRequest, String> {
    r.title = r.title.trim().replace(['\n', '\r'], " ");
    if r.title.is_empty() || r.title.chars().count() > 256 {
        return Err("a title has 1 to 256 characters".into());
    }
    if r.body.chars().count() > 65_536 {
        return Err("the description is longer than GitHub allows".into());
    }
    r.base = r.base.trim().to_owned();
    if !branch_name_ok(&r.base, false) {
        return Err(format!("\"{}\" is not a branch name", r.base));
    }
    r.branch = r.branch.trim().to_owned();
    if !branch_name_ok(&r.branch, false) {
        return Err(format!("\"{}\" is not a branch name", r.branch));
    }
    if r.branch == r.base {
        return Err("the branch and the base are the same".into());
    }
    r.commit_message = r
        .commit_message
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    Ok(r)
}

/// Whether `branch` is a name Blongo gave a worktree (the prefix and 12
/// hex digits), so a better one may be suggested.
fn generated_branch(branch: &str, prefix: &str) -> bool {
    branch
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == 12 && rest.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A branch name (or, with `prefix`, the start of one) git accepts and
/// that cannot be read as an option: letters, digits, `.`, `_`, `-`, `/`;
/// no `..`, `//`, `@{`, leading `-` / `/` / `.`, trailing `.lock` / `.`
/// (a prefix may end with `/`).
pub(super) fn branch_name_ok(name: &str, prefix: bool) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        && !name.starts_with(['-', '/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("/.")
        && !name.ends_with(".lock")
        && !name.ends_with('.')
        && (prefix || !name.ends_with('/'))
}

// ------------------------------------------------------------------ jobs

/// Look up and check one pull request (a link command).
pub(super) async fn link(
    ctx: ForgeCtx,
    cwd: PathBuf,
    reference: (Option<String>, Option<String>, u64),
) -> Result<Found, String> {
    let (host, repo_name, number) = reference;
    let origin = match remote::remote_url(&cwd).await {
        Some(url) => RepoRef::parse(&url),
        None => None,
    };
    let repo = match repo_name {
        Some(full) => {
            let origin_host = origin.as_ref().map(|o| o.host.clone());
            let host = host
                .or_else(|| origin_host.clone())
                .unwrap_or_else(|| "github.com".into());
            // A token goes only to github.com or the folder's own host, not
            // to whatever host a pasted URL names.
            if host != "github.com" && origin_host.as_deref() != Some(host.as_str()) {
                return Err(format!(
                    "{host} is not this folder's GitHub host; only github.com and the remote's \
                     host are linked"
                ));
            }
            RepoRef::parse(&format!("https://{host}/{full}")).ok_or("not a GitHub repository")?
        }
        None => {
            origin.ok_or("this folder has no GitHub remote; link the pull request by its URL")?
        }
    };
    let token = ctx.token(&repo.host).await?;
    let gh = ctx.client(&repo, token);
    let info = gh
        .repo_info(&repo)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    let pull = gh.pull(&repo, number).await.map_err(|e| match e {
        GhError::NotFound => format!("{}#{number} was not found", repo.full_name()),
        e => ctx.fail(&repo.host, e),
    })?;
    Ok(found(&gh, &repo, info, pull).await)
}

/// The link and first status of `pull`. The status falls back to what
/// REST said when the GraphQL check fails (the next poll retries).
async fn found(gh: &GitHub, repo: &RepoRef, info: RepoInfo, pull: PullInfo) -> Found {
    let status = match gh.statuses(repo, &[pull.number]).await {
        Ok((mut map, _)) => map.remove(&pull.number),
        Err(_) => None,
    }
    .unwrap_or_else(|| PrStatus {
        state: pull.state,
        title: pull.title.clone(),
        head_sha: pull.head_sha.clone(),
        ..PrStatus::default()
    });
    // GitHub answers with the canonical casing; remotes may differ.
    let read_only = !info.can_push
        || !pull
            .head_repo
            .as_deref()
            .is_some_and(|h| h.eq_ignore_ascii_case(&repo.full_name()));
    // Opened by the app: only a page on the repository's own host.
    let url_ok = pull
        .url
        .strip_prefix("https://")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(host, _)| host.eq_ignore_ascii_case(&repo.host));
    Found {
        link: PrLink {
            host: repo.host.clone(),
            repo: repo.full_name(),
            number: pull.number,
            url: if url_ok {
                pull.url.clone()
            } else {
                repo.pull_url(pull.number)
            },
            head_branch: pull.head_branch,
            base_branch: pull.base_branch,
            read_only,
        },
        status,
        info,
    }
}

async fn run_poll(ctx: ForgeCtx, polls: Vec<PollTarget>, lookups: Vec<LookupTarget>) -> ForgeDone {
    let mut done = ForgeDone {
        polled: Vec::new(),
        found: Vec::new(),
        rate: None,
    };
    let mut tokens: HashMap<String, Result<String, String>> = HashMap::new();
    // One request per repository (in batches of MAX_BATCH).
    let mut by_repo: HashMap<(String, String), Vec<PollTarget>> = HashMap::new();
    for target in polls {
        by_repo
            .entry((target.link.host.clone(), target.link.repo.clone()))
            .or_default()
            .push(target);
    }
    for ((host, full), targets) in by_repo {
        let repo = RepoRef::parse(&format!("https://{host}/{full}"));
        let token = match tokens.get(&host) {
            Some(t) => t.clone(),
            None => {
                let t = ctx.token(&host).await;
                tokens.insert(host.clone(), t.clone());
                t
            }
        };
        let (repo, token) = match (repo, token) {
            (Some(repo), Ok(token)) => (repo, token),
            (None, _) => {
                fail_all(&mut done, targets, "not a GitHub repository");
                continue;
            }
            (_, Err(err)) => {
                fail_all(&mut done, targets, &err);
                continue;
            }
        };
        let gh = ctx.client(&repo, token);
        for chunk in targets.chunks(MAX_BATCH) {
            // Several threads may share a pull request: ask once.
            let mut numbers: Vec<u64> = chunk.iter().map(|t| t.link.number).collect();
            numbers.sort_unstable();
            numbers.dedup();
            match gh.statuses(&repo, &numbers).await {
                Ok((map, rate)) => {
                    done.rate = rate.or(done.rate);
                    for t in chunk {
                        let result = map
                            .get(&t.link.number)
                            .cloned()
                            .ok_or_else(|| format!("{} was not found", t.link.label()));
                        done.polled.push((t.thread_id, t.link.clone(), result));
                    }
                }
                Err(err) => {
                    if matches!(err, GhError::RateLimited { .. }) {
                        done.rate = Some(RateLimit {
                            limit: 1,
                            remaining: 0,
                        });
                    }
                    let msg = ctx.fail(&host, err);
                    for t in chunk {
                        done.polled
                            .push((t.thread_id, t.link.clone(), Err(msg.clone())));
                    }
                }
            }
        }
    }
    for target in lookups {
        let result = look_up(&ctx, &target, &mut tokens).await;
        done.found.push((target.thread_id, result));
    }
    done
}

enum PrOp {
    Detail {
        cwd: PathBuf,
        auto_fix: Option<blongo_protocol::AutoFixInfo>,
        /// The repository's merge settings as known, and the method
        /// chosen last time.
        merge: (Option<RepoInfo>, Option<blongo_protocol::MergeMethod>),
    },
    Edit {
        title: Option<String>,
        body: Option<String>,
    },
    Prepare {
        cwd: PathBuf,
        branch: String,
        prefix: String,
        /// The project's base branch setting (`None`: GitHub's default).
        base: Option<String>,
        title: Option<String>,
    },
    Create {
        cwd: PathBuf,
        branch: String,
        request: PrCreateRequest,
    },
    Push {
        cwd: PathBuf,
        branch: String,
    },
}

async fn run_pr_op(
    ctx: &ForgeCtx,
    link: Option<&PrLink>,
    op: PrOp,
    id: QueryId,
    thread_id: ThreadId,
) -> QueryDone {
    let mut done = QueryDone::new(id, thread_id, Err(String::new()));
    match op {
        PrOp::Prepare {
            cwd,
            branch,
            prefix,
            base,
            title,
        } => {
            done.result = match prepare(ctx, &cwd, &branch, &prefix, base, title).await {
                Ok((prep, info)) => {
                    done.info = Some(info);
                    Ok(QueryReply::PrPrepare(Box::new(prep)))
                }
                Err(err) => Err(err),
            };
        }
        PrOp::Create {
            cwd,
            branch,
            request,
        } => {
            let (renamed, result) = create(ctx, &cwd, &branch, &request).await;
            done.renamed = renamed;
            done.result = result.map(|found| {
                let label = found.link.label();
                done.created = Some(Box::new(found));
                QueryReply::Done(format!("{label} created"))
            });
        }
        PrOp::Push { cwd, branch } => {
            done.result = push(&cwd, &branch).await.map(QueryReply::Done);
            done.changed = done.result.is_ok();
        }
        PrOp::Detail {
            cwd,
            auto_fix,
            merge: (info, last),
        } => {
            let Some(link) = link else {
                done.result = Err("no pull request is linked".into());
                return done;
            };
            let (gh, repo) = match client_for(ctx, link).await {
                Ok(c) => c,
                Err(err) => {
                    done.result = Err(err);
                    return done;
                }
            };
            done.result = match gh.detail(&repo, link.number).await {
                Ok(mut detail) => {
                    detail.can_edit &= !link.read_only;
                    if let Some((ahead, behind)) =
                        remote::ahead_behind(&cwd, &link.head_branch).await
                    {
                        detail.ahead = Some(ahead);
                        detail.behind = Some(behind);
                    }
                    detail.uncommitted = remote::uncommitted(&cwd).await;
                    detail.link = Some(link.clone());
                    detail.auto_fix = auto_fix;
                    // Which merge methods to offer: asked once, then cached.
                    let info = match info {
                        Some(info) => Some(info),
                        None => match gh.repo_info(&repo).await {
                            Ok(info) => {
                                done.info =
                                    Some((link.host.clone(), link.repo.clone(), info.clone()));
                                Some(info)
                            }
                            Err(_) => None,
                        },
                    };
                    super::merge::fill_merge_facts(&mut detail, info.as_ref(), last);
                    done.status = Some(detail.status.clone());
                    Ok(QueryReply::PrDetail(Box::new(detail)))
                }
                Err(GhError::NotFound) => Err(format!("{} was not found", link.label())),
                Err(err) => Err(ctx.fail(&repo.host, err)),
            };
        }
        PrOp::Edit { title, body } => {
            let Some(link) = link else {
                done.result = Err("no pull request is linked".into());
                return done;
            };
            let (gh, repo) = match client_for(ctx, link).await {
                Ok(c) => c,
                Err(err) => {
                    done.result = Err(err);
                    return done;
                }
            };
            done.result = match gh
                .edit_pull(&repo, link.number, title.as_deref(), body.as_deref())
                .await
            {
                Ok(()) => {
                    done.changed = true;
                    Ok(QueryReply::Done(format!("{} updated", link.label())))
                }
                Err(err) => Err(ctx.fail(&repo.host, err)),
            };
        }
    }
    done
}

pub(super) async fn client_for(ctx: &ForgeCtx, link: &PrLink) -> Result<(GitHub, RepoRef), String> {
    let repo = RepoRef::parse(&format!("https://{}/{}", link.host, link.repo))
        .ok_or("not a GitHub repository")?;
    let token = ctx.token(&repo.host).await?;
    Ok((ctx.client(&repo, token), repo))
}

/// The folder's GitHub repository, its remote's name and a client for it.
async fn folder_repo(ctx: &ForgeCtx, cwd: &Path) -> Result<(GitHub, RepoRef, String), String> {
    let name = remote::remote_name(cwd)
        .await
        .ok_or("this folder has no git remote to push to")?;
    let repo = remote::remote_url(cwd)
        .await
        .and_then(|u| RepoRef::parse(&u))
        .ok_or("this folder's remote is not a GitHub repository")?;
    let token = ctx.token(&repo.host).await?;
    Ok((ctx.client(&repo, token), repo, name))
}

/// What the Create PR form starts from.
async fn prepare(
    ctx: &ForgeCtx,
    cwd: &Path,
    branch: &str,
    prefix: &str,
    base: Option<String>,
    title: Option<String>,
) -> Result<(PrPrepare, (String, String, RepoInfo)), String> {
    let (gh, repo, remote_name) = folder_repo(ctx, cwd).await?;
    let info = gh
        .repo_info(&repo)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    let base = base.unwrap_or_else(|| info.default_branch.clone());
    if !branch_name_ok(&base, false) {
        return Err(format!("\"{base}\" cannot be used as a base branch here"));
    }
    // Compare with GitHub's base as it is now; the last fetch otherwise.
    let _ = branch::fetch(cwd, &remote_name, &base, BASE_FETCH).await;
    let base_ref = if branch::has_remote_branch(cwd, &remote_name, &base).await {
        format!("refs/remotes/{remote_name}/{base}")
    } else {
        base.clone()
    };
    let pushed = remote::branch_pushed(cwd, branch).await;
    let commits = branch::commits_since(cwd, &base_ref).await;
    let uncommitted = branch::uncommitted_files(cwd).await;
    let diff_stat = branch::diff_stat(cwd, &base_ref).await;
    let template = branch::template(cwd).await;
    let title = title
        .or_else(|| commits.first().cloned())
        .unwrap_or_default();
    let suggested_branch = if !pushed && generated_branch(branch, prefix) {
        let slug = branch_slug(&title);
        let name = format!("{prefix}{slug}");
        if slug.is_empty() || !branch_name_ok(&name, false) {
            branch.to_owned()
        } else {
            name
        }
    } else {
        branch.to_owned()
    };
    let draft_prompt = blongo_forge::pr::draft_prompt(&blongo_forge::pr::DraftFacts {
        branch: &suggested_branch,
        base: &base,
        commits: &commits,
        uncommitted: &uncommitted,
        diff_stat: &diff_stat,
        template: template.as_deref(),
    });
    let prep = PrPrepare {
        repo: repo.full_name(),
        branch: branch.to_owned(),
        pushed,
        suggested_branch,
        base,
        uncommitted,
        commits,
        diff_stat,
        template,
        title,
        draft_prompt,
        can_push: info.can_push,
    };
    Ok((prep, (repo.host.clone(), repo.full_name(), info)))
}

/// Commit, rename, push and open the pull request; each step is skipped
/// when it is done already, so a failed create can be retried. The new
/// branch name is reported even when a later step fails.
async fn create(
    ctx: &ForgeCtx,
    cwd: &Path,
    branch: &str,
    request: &PrCreateRequest,
) -> (Option<String>, Result<Found, String>) {
    let mut renamed = None;
    let result = async {
        let (gh, repo, remote_name) = folder_repo(ctx, cwd).await?;
        if remote::current_branch(cwd).await.as_deref() != Some(branch) {
            return Err(format!("the worktree does not have {branch} checked out"));
        }
        let files = branch::uncommitted_files(cwd).await;
        if !files.is_empty() {
            let message = request.commit_message.as_deref().ok_or_else(|| {
                format!(
                    "{} file{} not committed; commit them or enter a commit message",
                    files.len(),
                    if files.len() == 1 { " is" } else { "s are" }
                )
            })?;
            blongo_git::workspace::commit_all(cwd, message)
                .await
                .map_err(|e| format!("Committing failed: {e:#}"))?;
        }
        let mut name = branch.to_owned();
        let pushed = remote::branch_pushed(cwd, branch).await;
        if request.branch != branch && pushed {
            return Err(format!("{branch} was pushed already, so it keeps its name"));
        }
        // Never pushed: the name must be free on GitHub, or the push could
        // add to somebody else's branch (and pull request).
        if !pushed {
            let tip = branch::remote_branch_tip(cwd, &remote_name, &request.branch)
                .await
                .map_err(|e| format!("Checking the branch name on GitHub failed: {e}"))?;
            // At this very commit it is ours: an earlier push got through
            // although it was reported as failed (a timeout).
            let ours = tip.is_some() && request.branch == branch && tip == branch::head(cwd).await;
            if tip.is_some() && !ours {
                return Err(format!(
                    "{} exists on GitHub already; choose another branch name",
                    request.branch
                ));
            }
        }
        if request.branch != branch {
            branch::rename(cwd, branch, &request.branch)
                .await
                .map_err(|e| format!("Renaming the branch failed: {e}"))?;
            renamed = Some(request.branch.clone());
            name = request.branch.clone();
        }
        branch::push(cwd, &remote_name, &name)
            .await
            .map_err(|e| format!("Pushing failed: {e}"))?;
        let head = branch::head(cwd).await.unwrap_or_default();
        let info = gh
            .repo_info(&repo)
            .await
            .map_err(|e| ctx.fail(&repo.host, e))?;
        let new = NewPull {
            title: &request.title,
            body: &request.body,
            head: &name,
            base: &request.base,
            draft: request.draft,
        };
        let pull = match gh.create_pull(&repo, &new).await {
            Ok(Some(pull)) => pull,
            // Opened before (a retry, or by hand): link it only when it is
            // this branch as just pushed.
            Ok(None) => {
                let pull = gh
                    .find_pull(&repo, &name)
                    .await
                    .map_err(|e| ctx.fail(&repo.host, e))?
                    .ok_or(
                        "GitHub says a pull request exists for the branch but did not list it",
                    )?;
                if pull.state.is_final() || pull.head_sha != head {
                    return Err(format!(
                        "{}#{} is open for {name} but is not at this thread's commit; link it \
                         by hand if it is this work",
                        repo.full_name(),
                        pull.number
                    ));
                }
                pull
            }
            Err(err) => {
                return Err(format!(
                    "GitHub did not create the pull request: {}",
                    ctx.fail(&repo.host, err)
                ));
            }
        };
        Ok(found(&gh, &repo, info, pull).await)
    }
    .await;
    (renamed, result)
}

/// Push the checked-out `branch` (never forced).
async fn push(cwd: &Path, branch: &str) -> Result<String, String> {
    if remote::current_branch(cwd).await.as_deref() != Some(branch) {
        return Err(format!("the worktree does not have {branch} checked out"));
    }
    let remote_name = remote::remote_name(cwd)
        .await
        .ok_or("this folder has no git remote to push to")?;
    branch::push(cwd, &remote_name, branch)
        .await
        .map_err(|e| format!("Pushing failed: {e}"))?;
    Ok(format!("pushed {branch}"))
}

/// Where a new worktree starts, resolved on the loop.
pub(super) struct BasePlan {
    ctx: ForgeCtx,
    /// The project's base branch setting.
    custom: Option<String>,
    /// The base the project's last worktree started from.
    cached: Option<String>,
}

/// Where a new worktree starts: `start` (`None`: the project's HEAD).
#[derive(Default)]
pub(super) struct WorktreeBase {
    pub start: Option<String>,
    /// The base branch it started from.
    pub base: Option<String>,
    /// Why it starts from the project's HEAD although a base was known.
    pub notice: Option<String>,
}

/// Longest the default branch is asked of GitHub before the cached one
/// is used.
const BASE_LOOKUP: Duration = Duration::from_secs(8);
/// Longest the base branch is fetched before the last fetched one is
/// used (a new worktree waits for it).
const BASE_FETCH: Duration = Duration::from_secs(15);

/// The remote's base branch, freshly fetched: the project's setting, else
/// GitHub's default branch, else the last one used, else the remote's
/// HEAD. A folder with no remote starts from its HEAD as before.
pub(super) async fn worktree_base(root: &Path, plan: BasePlan) -> WorktreeBase {
    let Some(remote_name) = remote::remote_name(root).await else {
        return WorktreeBase::default();
    };
    let name = match plan.custom {
        Some(name) => Some(name),
        None => {
            let github = async {
                let repo = RepoRef::parse(&remote::remote_url(root).await?)?;
                let token = plan.ctx.token(&repo.host).await.ok()?;
                let gh = plan.ctx.client(&repo, token);
                gh.repo_info(&repo)
                    .await
                    .ok()
                    .map(|i| i.default_branch)
                    .filter(|b| !b.is_empty())
            };
            match tokio::time::timeout(BASE_LOOKUP, github).await {
                Ok(Some(name)) => Some(name),
                _ => match plan.cached {
                    Some(name) => Some(name),
                    None => branch::remote_head(root, &remote_name).await,
                },
            }
        }
    };
    let Some(name) = name else {
        return WorktreeBase::default();
    };
    if !branch_name_ok(&name, false) {
        return WorktreeBase {
            notice: Some(format!(
                "This worktree starts from the project's current commit: Blongo does not use \
                 \"{name}\" as a base branch."
            )),
            ..WorktreeBase::default()
        };
    }
    let fetched = branch::fetch(root, &remote_name, &name, BASE_FETCH).await;
    if branch::has_remote_branch(root, &remote_name, &name).await {
        return WorktreeBase {
            start: Some(format!("refs/remotes/{remote_name}/{name}")),
            notice: fetched.err().map(|err| {
                format!(
                    "{remote_name}/{name} could not be fetched, so this worktree starts from \
                     where it was last fetched: {err}"
                )
            }),
            base: Some(name),
        };
    }
    WorktreeBase {
        start: None,
        base: None,
        notice: Some(format!(
            "This worktree starts from the project's current commit: {remote_name}/{name} is not \
             available ({}).",
            fetched.err().unwrap_or_else(|| "no such branch".into())
        )),
    }
}

fn fail_all(done: &mut ForgeDone, targets: Vec<PollTarget>, err: &str) {
    for t in targets {
        done.polled.push((t.thread_id, t.link, Err(err.to_owned())));
    }
}

/// The pull request of a thread's branch, if it was pushed and has one.
async fn look_up(
    ctx: &ForgeCtx,
    target: &LookupTarget,
    tokens: &mut HashMap<String, Result<String, String>>,
) -> Result<Option<Found>, String> {
    let branch = match &target.branch {
        Some(b) => b.clone(),
        None => match remote::current_branch(&target.cwd).await {
            Some(b) => b,
            None => return Ok(None),
        },
    };
    if target
        .cached_info
        .as_ref()
        .is_some_and(|i| i.default_branch == branch)
    {
        return Ok(None);
    }
    // Local checks first: nothing pushed, nothing to ask GitHub.
    if !remote::branch_pushed(&target.cwd, &branch).await {
        return Ok(None);
    }
    let Some(repo) = remote::remote_url(&target.cwd)
        .await
        .and_then(|u| RepoRef::parse(&u))
    else {
        return Ok(None);
    };
    let token = match tokens.get(&repo.host) {
        Some(t) => t.clone(),
        None => {
            let t = ctx.token(&repo.host).await;
            tokens.insert(repo.host.clone(), t.clone());
            t
        }
    }?;
    let gh = ctx.client(&repo, token);
    let info = gh
        .repo_info(&repo)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    if info.default_branch == branch {
        return Ok(None);
    }
    let Some(pull) = gh
        .find_pull(&repo, &branch)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?
    else {
        return Ok(None);
    };
    Ok(Some(found(&gh, &repo, info, pull).await))
}

fn describe(err: GhError) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        for ok in ["main", "release/1.2", "blongo/x_y-z"] {
            assert!(branch_name_ok(ok, false), "{ok}");
        }
        assert!(branch_name_ok("blongo/", true));
        for bad in [
            "", "-x", "/x", ".x", "a..b", "a//b", "a/.b", "x.lock", "x.", "a b", "a~b", "a:b",
            "a^b", "a@{b", "dev/",
        ] {
            assert!(!branch_name_ok(bad, false), "{bad}");
        }
    }

    #[test]
    fn backoff_and_intervals() {
        assert_eq!(ForgeRt::backoff(1), Duration::from_secs(30));
        assert_eq!(ForgeRt::backoff(3), Duration::from_secs(120));
        assert_eq!(ForgeRt::backoff(30), MAX_BACKOFF);
        let mut rt = ForgeRt::default();
        let t = ThreadId::new();
        let now = Instant::now();
        let mut status = PrStatus::default();
        assert_eq!(rt.interval(t, &status, now), IDLE);
        status.checks.state = ChecksState::Pending;
        assert_eq!(rt.interval(t, &status, now), CHECKS_RUNNING);
        rt.hurry(t, Duration::ZERO);
        assert_eq!(rt.interval(t, &status, now), FAST);
        rt.low_rate = true;
        assert_eq!(rt.interval(t, &status, now), FAST * 2);
        assert!(rt.next_due().is_some());
        rt.forget(t);
        assert!(rt.next_due().is_none());
    }
}
