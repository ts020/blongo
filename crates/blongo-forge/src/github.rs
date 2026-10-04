//! The few GitHub API calls the PR link needs: the repository (default
//! branch, permissions), finding a branch's pull request, and the status
//! of many pull requests in one GraphQL request.
//!
//! Everything goes through [`crate::http`] (curl, token on stdin). Owner,
//! name and branch values come from [`crate::remote::RepoRef`] or GitHub
//! itself; they are percent-encoded or limited to safe characters before
//! they reach a URL or a query string.

use std::collections::HashMap;

use blongo_protocol::{
    CheckDetail, CheckState, ChecksState, ChecksSummary, MergeState, Mergeable, PrDetail, PrState,
    PrStatus, ReviewComment, ReviewDecision, ReviewDetail, ReviewState, ReviewThread,
};
use serde_json::{Value, json};

use crate::http::{self, Request};
use crate::remote::RepoRef;

/// Most pull requests asked for in one GraphQL request (GitHub's node
/// limits stay far away at this size).
pub const MAX_BATCH: usize = 50;

/// Why a call failed, coarse enough to decide what to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GhError {
    /// The token was refused (401) or lacks access (403 without a rate
    /// limit).
    Auth(String),
    NotFound,
    /// Primary or secondary rate limit. `reset_at` (Unix seconds) when
    /// GitHub said.
    RateLimited {
        reset_at: Option<i64>,
    },
    Other(String),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(m) => write!(f, "GitHub refused the token: {m}"),
            Self::NotFound => f.write_str("GitHub: not found"),
            Self::RateLimited { .. } => f.write_str("GitHub rate limit reached"),
            Self::Other(m) => f.write_str(m),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RepoInfo {
    pub default_branch: String,
    /// The token may push.
    pub can_push: bool,
    pub allow_merge_commit: bool,
    pub allow_squash_merge: bool,
    pub allow_rebase_merge: bool,
    pub delete_branch_on_merge: bool,
}

/// A pull request as REST returns it, reduced to what linking needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullInfo {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: PrState,
    pub head_branch: String,
    /// `owner/name` of the head repository (`None`: deleted fork).
    pub head_repo: Option<String>,
    pub head_sha: String,
    pub base_branch: String,
}

/// A pull request to open.
pub struct NewPull<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    pub draft: bool,
}

/// What GitHub reported of the GraphQL rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub limit: u32,
    pub remaining: u32,
}

impl RateLimit {
    /// Under a tenth of the budget left: poll less.
    pub fn is_low(&self) -> bool {
        self.limit > 0 && self.remaining.saturating_mul(10) < self.limit
    }
}

#[derive(Clone, Debug)]
pub struct GitHub {
    api: String,
    graphql: String,
    token: String,
}

impl GitHub {
    pub fn new(repo: &RepoRef, token: impl Into<String>) -> Self {
        Self {
            api: repo.api_base(),
            graphql: repo.graphql_url(),
            token: token.into(),
        }
    }

    /// Talk to `api` instead (tests' fake server); GraphQL at
    /// `{api}/graphql`.
    pub fn with_api(api: &str, token: impl Into<String>) -> Self {
        let api = api.trim_end_matches('/').to_owned();
        Self {
            graphql: format!("{api}/graphql"),
            api,
            token: token.into(),
        }
    }

    fn request(&self, req: Request) -> Request {
        req.header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "blongo")
    }

    async fn get(&self, path: &str) -> Result<Value, GhError> {
        let resp = http::send(self.request(Request::get(format!("{}{path}", self.api))))
            .await
            .map_err(GhError::Other)?;
        if !resp.ok() {
            return Err(classify(resp.status, &resp.text()));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|e| GhError::Other(format!("GitHub: bad JSON: {e}")))
    }

    /// `GET /repos/{owner}/{name}`.
    pub async fn repo_info(&self, repo: &RepoRef) -> Result<RepoInfo, GhError> {
        let v = self.get(&format!("/repos/{}", repo.full_name())).await?;
        let flag = |k: &str| v[k].as_bool().unwrap_or(false);
        Ok(RepoInfo {
            default_branch: v["default_branch"].as_str().unwrap_or("").to_owned(),
            can_push: v["permissions"]["push"].as_bool().unwrap_or(false),
            allow_merge_commit: v["allow_merge_commit"].as_bool().unwrap_or(true),
            allow_squash_merge: v["allow_squash_merge"].as_bool().unwrap_or(true),
            allow_rebase_merge: v["allow_rebase_merge"].as_bool().unwrap_or(true),
            delete_branch_on_merge: flag("delete_branch_on_merge"),
        })
    }

    /// `GET /repos/{owner}/{name}/pulls/{number}`.
    pub async fn pull(&self, repo: &RepoRef, number: u64) -> Result<PullInfo, GhError> {
        let v = self
            .get(&format!("/repos/{}/pulls/{number}", repo.full_name()))
            .await?;
        pull_info(&v).ok_or_else(|| GhError::Other("GitHub: unexpected pull request".into()))
    }

    /// The newest pull request whose head is `branch` in the repository
    /// itself (`state=all`: a merged one still links). `None`: there is
    /// none.
    pub async fn find_pull(
        &self,
        repo: &RepoRef,
        branch: &str,
    ) -> Result<Option<PullInfo>, GhError> {
        let head = format!("{}:{branch}", repo.owner);
        let v = self
            .get(&format!(
                "/repos/{}/pulls?state=all&per_page=5&head={}",
                repo.full_name(),
                encode(&head)
            ))
            .await?;
        let mut pulls: Vec<PullInfo> = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(pull_info)
            .filter(|p| p.head_branch == branch)
            .collect();
        // Prefer an open one, then the newest.
        pulls.sort_by_key(|p| (p.state.is_final(), std::cmp::Reverse(p.number)));
        Ok(pulls.into_iter().next())
    }

    /// Status of `numbers` (at most [`MAX_BATCH`]) in one request. A
    /// number missing from the map was not found (deleted, no access).
    pub async fn statuses(
        &self,
        repo: &RepoRef,
        numbers: &[u64],
    ) -> Result<(HashMap<u64, PrStatus>, Option<RateLimit>), GhError> {
        let v = self.graphql(&status_query(repo, numbers)).await?;
        parse_statuses(&v, numbers)
    }
}

impl GitHub {
    /// Everything the PR tab shows of pull request `number` (the link
    /// and the local fields are left for the caller).
    pub async fn detail(&self, repo: &RepoRef, number: u64) -> Result<PrDetail, GhError> {
        let v = self.graphql(&detail_query(repo, number)).await?;
        parse_detail(&v)
    }

    /// `PATCH /repos/{owner}/{name}/pulls/{number}` with a new title and/or
    /// body.
    pub async fn edit_pull(
        &self,
        repo: &RepoRef,
        number: u64,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<(), GhError> {
        let mut patch = serde_json::Map::new();
        if let Some(t) = title {
            patch.insert("title".into(), t.into());
        }
        if let Some(b) = body {
            patch.insert("body".into(), b.into());
        }
        let url = format!("{}/repos/{}/pulls/{number}", self.api, repo.full_name());
        let resp = http::send(self.request(Request::json("PATCH", url, &Value::Object(patch))))
            .await
            .map_err(GhError::Other)?;
        if !resp.ok() {
            return Err(classify(resp.status, &resp.text()));
        }
        Ok(())
    }

    /// `POST /repos/{owner}/{name}/pulls` from `head` (a branch of the
    /// repository itself) into `base`. `Ok(None)`: GitHub says one is open
    /// for `head` already.
    pub async fn create_pull(
        &self,
        repo: &RepoRef,
        new: &NewPull<'_>,
    ) -> Result<Option<PullInfo>, GhError> {
        let url = format!("{}/repos/{}/pulls", self.api, repo.full_name());
        let body = json!({
            "title": new.title,
            "body": new.body,
            "head": new.head,
            "base": new.base,
            "draft": new.draft,
        });
        let resp = http::send(self.request(Request::json("POST", url, &body)))
            .await
            .map_err(GhError::Other)?;
        if resp.status == 422 && resp.text().contains("already exists") {
            return Ok(None);
        }
        if !resp.ok() {
            return Err(classify(resp.status, &unprocessable(&resp.text())));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| GhError::Other(format!("GitHub: bad JSON: {e}")))?;
        pull_info(&v)
            .map(Some)
            .ok_or_else(|| GhError::Other("GitHub: unexpected pull request".into()))
    }

    async fn graphql(&self, query: &str) -> Result<Value, GhError> {
        let resp = http::send(self.request(Request::post_json(
            self.graphql.clone(),
            &json!({ "query": query }),
        )))
        .await
        .map_err(GhError::Other)?;
        if !resp.ok() {
            return Err(classify(resp.status, &resp.text()));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|e| GhError::Other(format!("GitHub: bad JSON: {e}")))
    }
}

/// Fetched for the PR tab: the status fields plus names, links and times
/// of checks, latest reviews and review threads with their comments.
const DETAIL_FIELDS: &str = "number title body url state isDraft author{login} headRefOid \
mergeable mergeStateStatus reviewDecision additions deletions changedFiles viewerCanUpdate \
commits(last:1){nodes{commit{statusCheckRollup{state contexts(first:100){totalCount nodes{__typename \
... on CheckRun{name status conclusion detailsUrl startedAt completedAt \
checkSuite{workflowRun{workflow{name}}}} \
... on StatusContext{context state description targetUrl createdAt}}}}}}} \
latestReviews(first:30){nodes{author{login} state}} \
reviewThreads(first:100){totalCount nodes{id isResolved isOutdated path line \
comments(first:20){totalCount nodes{author{login} body createdAt}}}}";

/// GitHub allows 65,536 characters, up to four bytes each: a body it
/// accepts is never cut.
const MAX_BODY: usize = 4 * 65_536;
const MAX_COMMENT: usize = 8 * 1024;

fn detail_query(repo: &RepoRef, number: u64) -> String {
    format!(
        "query{{repository(owner:\"{}\",name:\"{}\"){{pullRequest(number:{number}){{{DETAIL_FIELDS}}}}}}}",
        repo.owner, repo.name
    )
}

fn parse_detail(v: &Value) -> Result<PrDetail, GhError> {
    let p = &v["data"]["repository"]["pullRequest"];
    if !p.is_object() {
        return Err(graphql_error(v));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let text = |v: &Value, max: usize| clip(v.as_str().unwrap_or(""), max);
    let login = |v: &Value| clip(v["login"].as_str().unwrap_or("ghost"), 100);
    let rollup = &p["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let contexts = rollup["contexts"]["nodes"].as_array();
    let checks: Vec<CheckDetail> = contexts
        .into_iter()
        .flatten()
        .map(|c| check_detail(c, now))
        .collect();
    let total_checks = rollup["contexts"]["totalCount"].as_u64().unwrap_or(0) as usize;
    let reviews = p["latestReviews"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let state = match r["state"].as_str()? {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "COMMENTED" => ReviewState::Commented,
                "DISMISSED" => ReviewState::Dismissed,
                _ => ReviewState::Pending,
            };
            Some(ReviewDetail {
                author: login(&r["author"]),
                state,
            })
        })
        .collect();
    let thread_nodes = p["reviewThreads"]["nodes"].as_array();
    let threads: Vec<ReviewThread> = thread_nodes
        .into_iter()
        .flatten()
        .map(|t| {
            let comments: Vec<ReviewComment> = t["comments"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| ReviewComment {
                    author: login(&c["author"]),
                    body: text(&c["body"], MAX_COMMENT),
                    created_at: text(&c["createdAt"], 40),
                })
                .collect();
            let total = t["comments"]["totalCount"].as_u64().unwrap_or(0) as usize;
            ReviewThread {
                id: text(&t["id"], 200),
                path: text(&t["path"], 1024),
                line: t["line"].as_u64().map(|l| l as u32),
                resolved: t["isResolved"].as_bool().unwrap_or(false),
                outdated: t["isOutdated"].as_bool().unwrap_or(false),
                more: total.saturating_sub(comments.len()) as u32,
                comments,
            }
        })
        .collect();
    let total_threads = p["reviewThreads"]["totalCount"].as_u64().unwrap_or(0) as usize;
    let merge_state = match p["mergeStateStatus"].as_str() {
        Some("CLEAN") => MergeState::Clean,
        Some("UNSTABLE") => MergeState::Unstable,
        Some("HAS_HOOKS") => MergeState::HasHooks,
        Some("BLOCKED") => MergeState::Blocked,
        Some("BEHIND") => MergeState::Behind,
        Some("DIRTY") => MergeState::Dirty,
        Some("DRAFT") => MergeState::Draft,
        _ => MergeState::Unknown,
    };
    let num = |k: &str| p[k].as_u64().unwrap_or(0) as u32;
    Ok(PrDetail {
        link: None,
        status: pr_status(p),
        body: text(&p["body"], MAX_BODY),
        body_truncated: p["body"].as_str().is_some_and(|b| b.len() > MAX_BODY),
        author: login(&p["author"]),
        merge_state,
        additions: num("additions"),
        deletions: num("deletions"),
        changed_files: num("changedFiles"),
        more_checks: total_checks.saturating_sub(checks.len()) as u32,
        checks,
        reviews,
        more_threads: total_threads.saturating_sub(threads.len()) as u32,
        threads,
        can_edit: p["viewerCanUpdate"].as_bool().unwrap_or(false),
        ahead: None,
        behind: None,
        uncommitted: 0,
    })
}

fn check_detail(c: &Value, now: i64) -> CheckDetail {
    let text = |k: &str| c[k].as_str().unwrap_or("");
    let state = match check_outcome(c) {
        Outcome::Pending => CheckState::Pending,
        Outcome::Failed => CheckState::Failure,
        Outcome::Passed => match (text("conclusion"), text("state")) {
            ("SUCCESS", _) | (_, "SUCCESS") => CheckState::Success,
            _ => CheckState::Neutral,
        },
    };
    let is_run = c["__typename"].as_str() == Some("CheckRun");
    let (name, detail, url) = if is_run {
        let detail = match c["conclusion"].as_str() {
            Some(conclusion) => conclusion,
            None => text("status"),
        };
        (text("name"), detail, text("detailsUrl"))
    } else {
        let detail = match text("description") {
            "" => text("state"),
            d => d,
        };
        (text("context"), detail, text("targetUrl"))
    };
    let started = parse_time(if is_run {
        text("startedAt")
    } else {
        text("createdAt")
    });
    let ended = parse_time(text("completedAt"));
    let duration_secs = match (started, ended) {
        (Some(s), Some(e)) if e >= s => Some((e - s) as u64),
        (Some(s), None) if state == CheckState::Pending && now >= s => Some((now - s) as u64),
        _ => None,
    };
    CheckDetail {
        name: clip(name, 200),
        workflow: c["checkSuite"]["workflowRun"]["workflow"]["name"]
            .as_str()
            .map(|w| clip(w, 200)),
        state,
        detail: clip(&detail.to_ascii_lowercase(), 200),
        url: Some(url)
            .filter(|u| u.starts_with("https://") && u.len() < 2048)
            .map(str::to_owned),
        duration_secs,
    }
}

/// `2026-10-04T12:34:56Z` (or with a fraction / offset `+00:00`) as Unix
/// seconds. Only UTC offsets of whole minutes are understood.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "Z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            sign * (r.get(1..3)?.parse::<i64>().ok()? * 3600
                + r.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// At most `max` bytes of `s` (cut at a character boundary).
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn graphql_error(v: &Value) -> GhError {
    let errors = v["errors"].as_array();
    let msg = errors
        .and_then(|e| e.first())
        .and_then(|e| e["message"].as_str())
        .unwrap_or("no data");
    let kind = errors
        .and_then(|e| e.first())
        .and_then(|e| e["type"].as_str())
        .unwrap_or("");
    match kind {
        "NOT_FOUND" => GhError::NotFound,
        "RATE_LIMITED" => GhError::RateLimited { reset_at: None },
        "FORBIDDEN" => GhError::Auth(short(msg)),
        _ => GhError::Other(format!("GitHub: {}", short(msg))),
    }
}

fn pull_info(v: &Value) -> Option<PullInfo> {
    let state = if v["merged_at"].is_string() || v["merged"].as_bool() == Some(true) {
        PrState::Merged
    } else if v["state"].as_str() == Some("closed") {
        PrState::Closed
    } else if v["draft"].as_bool() == Some(true) {
        PrState::Draft
    } else {
        PrState::Open
    };
    Some(PullInfo {
        number: v["number"].as_u64()?,
        url: v["html_url"].as_str().unwrap_or("").to_owned(),
        title: v["title"].as_str().unwrap_or("").to_owned(),
        state,
        head_branch: v["head"]["ref"].as_str()?.to_owned(),
        head_repo: v["head"]["repo"]["full_name"].as_str().map(str::to_owned),
        head_sha: v["head"]["sha"].as_str().unwrap_or("").to_owned(),
        base_branch: v["base"]["ref"].as_str()?.to_owned(),
    })
}

const PR_FIELDS: &str = "number title state isDraft headRefOid mergeable reviewDecision \
commits(last:1){nodes{commit{statusCheckRollup{state contexts(first:100){nodes{__typename \
... on CheckRun{status conclusion} ... on StatusContext{state}}}}}}} \
reviewThreads(first:100){nodes{isResolved}}";

fn status_query(repo: &RepoRef, numbers: &[u64]) -> String {
    // Owner and name are limited to [A-Za-z0-9._-] by RepoRef::parse, so
    // they need no escaping inside the quotes.
    let mut q = format!(
        "query{{rateLimit{{limit remaining}} repository(owner:\"{}\",name:\"{}\"){{",
        repo.owner, repo.name
    );
    for n in numbers.iter().take(MAX_BATCH) {
        q.push_str(&format!(" p{n}:pullRequest(number:{n}){{{PR_FIELDS}}}"));
    }
    q.push_str("}}");
    q
}

fn parse_statuses(
    v: &Value,
    numbers: &[u64],
) -> Result<(HashMap<u64, PrStatus>, Option<RateLimit>), GhError> {
    let rate = v["data"]["rateLimit"].as_object().map(|r| RateLimit {
        limit: r.get("limit").and_then(Value::as_u64).unwrap_or(0) as u32,
        remaining: r.get("remaining").and_then(Value::as_u64).unwrap_or(0) as u32,
    });
    let repo = &v["data"]["repository"];
    if repo.is_null() {
        return Err(graphql_error(v));
    }
    let mut out = HashMap::new();
    for n in numbers.iter().take(MAX_BATCH) {
        let p = &repo[format!("p{n}")];
        if p.is_object() {
            out.insert(*n, pr_status(p));
        }
    }
    Ok((out, rate))
}

fn pr_status(p: &Value) -> PrStatus {
    let state = match p["state"].as_str() {
        Some("MERGED") => PrState::Merged,
        Some("CLOSED") => PrState::Closed,
        _ if p["isDraft"].as_bool() == Some(true) => PrState::Draft,
        _ => PrState::Open,
    };
    let review = match p["reviewDecision"].as_str() {
        Some("APPROVED") => ReviewDecision::Approved,
        Some("CHANGES_REQUESTED") => ReviewDecision::ChangesRequested,
        Some("REVIEW_REQUIRED") => ReviewDecision::ReviewRequired,
        _ => ReviewDecision::None,
    };
    let mergeable = match p["mergeable"].as_str() {
        Some("MERGEABLE") => Mergeable::Mergeable,
        Some("CONFLICTING") => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    };
    let rollup = &p["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let mut checks = ChecksSummary::default();
    for c in rollup["contexts"]["nodes"].as_array().into_iter().flatten() {
        checks.total += 1;
        match check_outcome(c) {
            Outcome::Pending => checks.pending += 1,
            Outcome::Failed => checks.failed += 1,
            Outcome::Passed => {}
        }
    }
    checks.state = if checks.total == 0 && rollup.is_null() {
        ChecksState::None
    } else if checks.failed > 0 {
        ChecksState::Failure
    } else if checks.pending > 0 {
        ChecksState::Pending
    } else {
        // More contexts than the first page: trust GitHub's rollup.
        match rollup["state"].as_str() {
            Some("FAILURE") | Some("ERROR") => ChecksState::Failure,
            Some("PENDING") | Some("EXPECTED") => ChecksState::Pending,
            _ if checks.total == 0 => ChecksState::None,
            _ => ChecksState::Success,
        }
    };
    let unresolved_threads = p["reviewThreads"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["isResolved"].as_bool() == Some(false))
        .count() as u32;
    PrStatus {
        state,
        title: p["title"].as_str().unwrap_or("").to_owned(),
        head_sha: p["headRefOid"].as_str().unwrap_or("").to_owned(),
        checks,
        review,
        mergeable,
        unresolved_threads,
        error: None,
    }
}

enum Outcome {
    Pending,
    Failed,
    Passed,
}

fn check_outcome(c: &Value) -> Outcome {
    match c["__typename"].as_str() {
        Some("CheckRun") => {
            if c["status"].as_str() != Some("COMPLETED") {
                return Outcome::Pending;
            }
            match c["conclusion"].as_str() {
                Some(
                    "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE",
                ) => Outcome::Failed,
                _ => Outcome::Passed,
            }
        }
        _ => match c["state"].as_str() {
            Some("PENDING" | "EXPECTED") => Outcome::Pending,
            Some("FAILURE" | "ERROR") => Outcome::Failed,
            _ => Outcome::Passed,
        },
    }
}

fn classify(status: u16, body: &str) -> GhError {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let message = v
        .as_ref()
        .and_then(|v| v["message"].as_str())
        .map(short)
        .unwrap_or_else(|| short(body));
    let rate = message.to_ascii_lowercase().contains("rate limit");
    match status {
        401 => GhError::Auth(message),
        403 | 429 if rate || status == 429 => GhError::RateLimited { reset_at: None },
        403 => GhError::Auth(message),
        404 => GhError::NotFound,
        _ => GhError::Other(format!("GitHub: HTTP {status}: {message}")),
    }
}

/// A 422's validation errors folded into its message ("Validation
/// Failed: No commits between main and x").
fn unprocessable(body: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    let details: Vec<String> = v["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str().or_else(|| e["code"].as_str()))
        .map(str::to_owned)
        .collect();
    if !details.is_empty() {
        let message = format!(
            "{}: {}",
            v["message"].as_str().unwrap_or("Validation Failed"),
            details.join("; ")
        );
        v["message"] = Value::String(message);
    }
    v.to_string()
}

/// At most 200 characters of an error message, on one line.
fn short(s: &str) -> String {
    s.chars()
        .take(200)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Percent-encode a query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        RepoRef::parse("https://github.com/ts020/blongo").unwrap()
    }

    #[test]
    fn query_batches_aliases() {
        let q = status_query(&repo(), &[3, 12]);
        assert!(q.starts_with(
            "query{rateLimit{limit remaining} repository(owner:\"ts020\",name:\"blongo\"){"
        ));
        assert!(q.contains(" p3:pullRequest(number:3){number title"));
        assert!(q.contains(" p12:pullRequest(number:12){"));
        assert!(q.ends_with("}}"));
        let many: Vec<u64> = (1..200).collect();
        assert_eq!(
            status_query(&repo(), &many).matches("pullRequest(").count(),
            MAX_BATCH
        );
    }

    #[test]
    fn parses_status_rollups() {
        let v = json!({"data": {
            "rateLimit": {"limit": 5000, "remaining": 100},
            "repository": {
                "p1": {
                    "number": 1, "title": "Fix", "state": "OPEN", "isDraft": false,
                    "headRefOid": "abc", "mergeable": "MERGEABLE",
                    "reviewDecision": "APPROVED",
                    "commits": {"nodes": [{"commit": {"statusCheckRollup": {
                        "state": "FAILURE",
                        "contexts": {"nodes": [
                            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
                            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "TIMED_OUT"},
                            {"__typename": "CheckRun", "status": "IN_PROGRESS", "conclusion": null},
                            {"__typename": "StatusContext", "state": "PENDING"},
                            {"__typename": "StatusContext", "state": "SUCCESS"}
                        ]}
                    }}}]},
                    "reviewThreads": {"nodes": [{"isResolved": true}, {"isResolved": false}]}
                },
                "p2": {
                    "number": 2, "title": "Draft", "state": "OPEN", "isDraft": true,
                    "headRefOid": "def", "mergeable": "UNKNOWN", "reviewDecision": null,
                    "commits": {"nodes": [{"commit": {"statusCheckRollup": null}}]},
                    "reviewThreads": {"nodes": []}
                },
                "p3": null
            }
        }});
        let (map, rate) = parse_statuses(&v, &[1, 2, 3]).unwrap();
        let rate = rate.unwrap();
        assert!(rate.is_low());
        let s1 = &map[&1];
        assert_eq!(s1.state, PrState::Open);
        assert_eq!(s1.review, ReviewDecision::Approved);
        assert_eq!(s1.mergeable, Mergeable::Mergeable);
        assert_eq!(
            s1.checks,
            ChecksSummary {
                state: ChecksState::Failure,
                total: 5,
                failed: 1,
                pending: 2
            }
        );
        assert_eq!(s1.unresolved_threads, 1);
        let s2 = &map[&2];
        assert_eq!(s2.state, PrState::Draft);
        assert_eq!(s2.checks.state, ChecksState::None);
        assert!(!map.contains_key(&3));
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-10-04T12:00:00Z"), Some(1_791_115_200));
        assert_eq!(parse_time("2026-10-04T12:00:00.123Z"), Some(1_791_115_200));
        assert_eq!(parse_time("2026-10-04T21:00:00+09:00"), Some(1_791_115_200));
        assert_eq!(parse_time("2026-13-04T12:00:00Z"), None);
        assert_eq!(parse_time("yesterday"), None);
        assert_eq!(clip("héllo", 2), "h…");
    }

    #[test]
    fn parses_details() {
        let v = json!({"data": {"repository": {"pullRequest": {
            "number": 5, "title": "T", "body": "Body", "state": "OPEN", "isDraft": false,
            "author": {"login": "octo"}, "headRefOid": "abc", "mergeable": "MERGEABLE",
            "mergeStateStatus": "BLOCKED", "reviewDecision": "CHANGES_REQUESTED",
            "additions": 10, "deletions": 2, "changedFiles": 3, "viewerCanUpdate": true,
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE",
                "contexts": {"totalCount": 3, "nodes": [
                    {"__typename": "CheckRun", "name": "test", "status": "COMPLETED",
                     "conclusion": "FAILURE", "detailsUrl": "https://github.com/x/y/actions/runs/1",
                     "startedAt": "2026-10-04T12:00:00Z", "completedAt": "2026-10-04T12:01:30Z",
                     "checkSuite": {"workflowRun": {"workflow": {"name": "CI"}}}},
                    {"__typename": "StatusContext", "context": "deploy", "state": "SUCCESS",
                     "description": "Deployed", "targetUrl": "javascript:alert(1)",
                     "createdAt": "2026-10-04T12:00:00Z"}
                ]}}}}]},
            "latestReviews": {"nodes": [{"author": {"login": "rev"}, "state": "CHANGES_REQUESTED"}]},
            "reviewThreads": {"totalCount": 1, "nodes": [{"id": "T1", "isResolved": false,
                "isOutdated": false, "path": "src/a.rs", "line": 4,
                "comments": {"totalCount": 2, "nodes": [{"author": null, "body": "Why?",
                    "createdAt": "2026-10-04T12:05:00Z"}]}}]}
        }}}});
        let d = parse_detail(&v).unwrap();
        assert_eq!(d.status.title, "T");
        assert_eq!(d.status.unresolved_threads, 1);
        assert_eq!(d.status.checks.failed, 1);
        assert_eq!(d.merge_state, MergeState::Blocked);
        assert_eq!((d.additions, d.deletions, d.changed_files), (10, 2, 3));
        assert!(d.can_edit);
        assert_eq!(d.more_checks, 1);
        let test = &d.checks[0];
        assert_eq!(test.state, CheckState::Failure);
        assert_eq!(test.workflow.as_deref(), Some("CI"));
        assert_eq!(test.duration_secs, Some(90));
        assert_eq!(test.detail, "failure");
        let deploy = &d.checks[1];
        assert_eq!(deploy.state, CheckState::Success);
        assert_eq!(deploy.detail, "deployed");
        assert_eq!(deploy.url, None, "only https links");
        assert_eq!(d.reviews[0].state, ReviewState::ChangesRequested);
        let t = &d.threads[0];
        assert_eq!((t.path.as_str(), t.line, t.more), ("src/a.rs", Some(4), 1));
        assert_eq!(t.comments[0].author, "ghost");
        let mut long = v.clone();
        long["data"]["repository"]["pullRequest"]["body"] = json!("界".repeat(65_536));
        let d = parse_detail(&long).unwrap();
        assert!(!d.body_truncated, "a body GitHub accepts is kept whole");
        assert_eq!(d.body.chars().count(), 65_536);
        long["data"]["repository"]["pullRequest"]["body"] = json!("x".repeat(MAX_BODY + 1));
        let d = parse_detail(&long).unwrap();
        assert!(d.body_truncated);
        let missing = json!({"data": {"repository": {"pullRequest": null}},
            "errors": [{"type": "NOT_FOUND", "message": "x"}]});
        assert_eq!(parse_detail(&missing).unwrap_err(), GhError::NotFound);
    }

    #[test]
    fn classifies_errors() {
        assert!(matches!(
            classify(401, r#"{"message":"Bad credentials"}"#),
            GhError::Auth(_)
        ));
        assert!(matches!(
            classify(403, r#"{"message":"API rate limit exceeded for user"}"#),
            GhError::RateLimited { .. }
        ));
        assert!(matches!(classify(429, ""), GhError::RateLimited { .. }));
        assert!(matches!(
            classify(403, r#"{"message":"Resource not accessible"}"#),
            GhError::Auth(_)
        ));
        assert_eq!(classify(404, "{}"), GhError::NotFound);
        let v = json!({"data": {"repository": null}, "errors": [{"type": "NOT_FOUND", "message": "x"}]});
        assert_eq!(parse_statuses(&v, &[1]).unwrap_err(), GhError::NotFound);
    }

    #[test]
    fn pull_info_states() {
        let base = |extra: Value| {
            let mut v = json!({"number": 4, "html_url": "u", "title": "t", "state": "open",
                "head": {"ref": "b", "sha": "s", "repo": {"full_name": "a/b"}}, "base": {"ref": "main"}});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            pull_info(&v).unwrap()
        };
        assert_eq!(base(json!({})).state, PrState::Open);
        assert_eq!(base(json!({"draft": true})).state, PrState::Draft);
        assert_eq!(base(json!({"state": "closed"})).state, PrState::Closed);
        assert_eq!(
            base(json!({"state": "closed", "merged_at": "2026"})).state,
            PrState::Merged
        );
        assert_eq!(encode("ts020:blongo/x y"), "ts020%3Ablongo%2Fx%20y");
    }
}
