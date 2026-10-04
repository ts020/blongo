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

use blongo_forge::auth;
use blongo_forge::github::{GhError, GitHub, MAX_BATCH, PullInfo, RateLimit, RepoInfo};
use blongo_forge::remote::{self, RepoRef};
use blongo_protocol::{BaseBranch, ChecksState, ForgeSettings, PrLink, PrStatus, parse_pr_ref};

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
    fn client(&self, repo: &RepoRef, token: String) -> GitHub {
        match &self.api {
            Some(api) => GitHub::with_api(api, token),
            None => GitHub::new(repo, token),
        }
    }

    async fn token(&self, host: &str) -> Result<String, String> {
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
    fn fail(&self, host: &str, err: GhError) -> String {
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
    }

    /// Something happened on the thread: poll it soon and fast for a while.
    fn hurry(&mut self, thread_id: ThreadId, after: Duration) {
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
    fn forge_ctx(&self) -> ForgeCtx {
        ForgeCtx {
            tokens: self.config.forge_tokens.clone(),
            gh: self.config.gh_program.clone(),
            api: self.config.github_api.clone(),
            cache: self.forge.tokens.clone(),
        }
    }

    fn cached_repo_info(&self, host: &str, repo: &str) -> Option<RepoInfo> {
        let (json, _) = self
            .store
            .forge_cache(&format!("repo:{host}/{repo}"))
            .ok()
            .flatten()?;
        serde_json::from_str(&json).ok()
    }

    fn cache_repo_info(&self, link: &PrLink, info: &RepoInfo) {
        if let Ok(json) = serde_json::to_string(info) {
            let key = format!("repo:{}/{}", link.host, link.repo);
            if let Err(err) = self.store.set_forge_cache(&key, &json) {
                eprintln!("blongo-core: forge cache: {err:#}");
            }
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
            EventKind::ThreadArchived { thread_id } => {
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
                Ok(_) => answers.push((thread_id, Ok("no pull request for this branch".into()))),
                Err(err) => answers.push((thread_id, Err(err))),
            }
        }
        self.commit_events(events);
        for (thread_id, result) in answers {
            self.answer_pr_waiters(thread_id, result);
        }
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
            manual: false,
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
        if !(1..=10).contains(&settings.auto_fix_max) {
            return Err("automatic CI fixes stop after 1 to 10 attempts".into());
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
fn owns_branch(thread: &Thread) -> bool {
    thread.worktree.is_some()
        && !thread.pr_dismissed
        && thread.parent_thread_id.is_none()
        && thread.forked_from.is_none()
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
