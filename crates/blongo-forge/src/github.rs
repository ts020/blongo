//! The few GitHub API calls the PR link needs: the repository (default
//! branch, permissions), finding a branch's pull request, and the status
//! of many pull requests in one GraphQL request.
//!
//! Everything goes through [`crate::http`] (curl, token on stdin). Owner,
//! name and branch values come from [`crate::remote::RepoRef`] or GitHub
//! itself; they are percent-encoded or limited to safe characters before
//! they reach a URL or a query string.

use std::collections::HashMap;

use blongo_protocol::{ChecksState, ChecksSummary, Mergeable, PrState, PrStatus, ReviewDecision};
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
        let query = status_query(repo, numbers);
        let resp = http::send(self.request(Request::post_json(
            self.graphql.clone(),
            &json!({ "query": query }),
        )))
        .await
        .map_err(GhError::Other)?;
        if !resp.ok() {
            return Err(classify(resp.status, &resp.text()));
        }
        let v: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| GhError::Other(format!("GitHub: bad JSON: {e}")))?;
        parse_statuses(&v, numbers)
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
        let errors = v["errors"].as_array();
        let msg = errors
            .and_then(|e| e.first())
            .and_then(|e| e["message"].as_str())
            .unwrap_or("no data");
        let kind = errors
            .and_then(|e| e.first())
            .and_then(|e| e["type"].as_str())
            .unwrap_or("");
        return Err(match kind {
            "NOT_FOUND" => GhError::NotFound,
            "RATE_LIMITED" => GhError::RateLimited { reset_at: None },
            "FORBIDDEN" => GhError::Auth(short(msg)),
            _ => GhError::Other(format!("GitHub: {}", short(msg))),
        });
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
