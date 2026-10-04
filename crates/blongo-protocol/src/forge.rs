//! A thread's pull request on GitHub: the link, the status Blongo polls,
//! and a project's forge settings.
//!
//! One thread = one worktree = one branch = one pull request. The status
//! is a summary small enough to live on the thread (the sidebar badge);
//! details (check names, review comments) are fetched on demand.

use serde::{Deserialize, Serialize};

/// The pull request a thread is linked to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrLink {
    /// Lowercased host (`github.com`, a GitHub Enterprise host).
    pub host: String,
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    /// Web URL of the pull request.
    pub url: String,
    pub head_branch: String,
    pub base_branch: String,
    /// The token cannot push to the repository (a fork's PR, read-only
    /// access): Blongo shows the status but offers no write actions.
    #[serde(default)]
    pub read_only: bool,
}

impl PrLink {
    /// `owner/name#123`.
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    #[default]
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrState {
    /// Merged and closed pull requests are not polled any more.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Merged | Self::Closed)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    /// The head commit has no checks.
    #[default]
    None,
    Pending,
    Success,
    Failure,
}

/// The head commit's checks (GitHub check runs and commit statuses).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChecksSummary {
    pub state: ChecksState,
    pub total: u32,
    pub failed: u32,
    pub pending: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// No review required (or none recorded).
    #[default]
    None,
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mergeable {
    /// GitHub has not computed it yet.
    #[default]
    Unknown,
    Mergeable,
    Conflicting,
}

/// What Blongo last saw of a linked pull request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrStatus {
    pub state: PrState,
    pub title: String,
    /// Head commit the checks belong to.
    pub head_sha: String,
    pub checks: ChecksSummary,
    pub review: ReviewDecision,
    pub mergeable: Mergeable,
    /// Review threads not resolved yet.
    pub unresolved_threads: u32,
    /// The last poll failed (network, token, rate limit); the other fields
    /// are what was seen before. Short, no secrets.
    #[serde(default)]
    pub error: Option<String>,
}

/// The one-word summary the sidebar shows, most pressing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrBadge {
    Merged,
    Closed,
    Conflict,
    ChecksFailing,
    ChangesRequested,
    ChecksRunning,
    ReadyToMerge,
    Draft,
    Open,
}

impl PrBadge {
    pub fn label(self) -> &'static str {
        match self {
            Self::Merged => "Merged",
            Self::Closed => "Closed",
            Self::Conflict => "Conflict",
            Self::ChecksFailing => "Checks failing",
            Self::ChangesRequested => "Changes requested",
            Self::ChecksRunning => "Checks running",
            Self::ReadyToMerge => "Ready to merge",
            Self::Draft => "Draft",
            Self::Open => "Open",
        }
    }
}

impl PrStatus {
    pub fn badge(&self) -> PrBadge {
        match self.state {
            PrState::Merged => return PrBadge::Merged,
            PrState::Closed => return PrBadge::Closed,
            PrState::Open | PrState::Draft => {}
        }
        if self.mergeable == Mergeable::Conflicting {
            return PrBadge::Conflict;
        }
        if self.checks.state == ChecksState::Failure {
            return PrBadge::ChecksFailing;
        }
        if self.review == ReviewDecision::ChangesRequested {
            return PrBadge::ChangesRequested;
        }
        if self.checks.state == ChecksState::Pending {
            return PrBadge::ChecksRunning;
        }
        if self.state == PrState::Draft {
            return PrBadge::Draft;
        }
        let review_ok = matches!(self.review, ReviewDecision::Approved | ReviewDecision::None);
        if review_ok && self.mergeable == Mergeable::Mergeable {
            return PrBadge::ReadyToMerge;
        }
        PrBadge::Open
    }
}

/// Which branch new worktrees start from and pull requests target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BaseBranch {
    /// The repository's default branch on GitHub (fallback: the last one
    /// seen, then the remote's `HEAD`).
    #[default]
    GithubDefault,
    Custom {
        name: String,
    },
}

/// A project's forge settings. Every field has a default, so projects
/// created before these existed read as the defaults.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ForgeSettings {
    pub base_branch: BaseBranch,
    /// Prefix of branches Blongo names (`blongo/`).
    pub branch_prefix: String,
    /// When a linked pull request's checks fail, send the failure to the
    /// thread's agent and let it push a fix.
    pub auto_fix_ci: bool,
    /// Consecutive failed auto-fix attempts before Blongo stops and asks.
    pub auto_fix_max: u32,
}

pub const DEFAULT_BRANCH_PREFIX: &str = "blongo/";

impl Default for ForgeSettings {
    fn default() -> Self {
        Self {
            base_branch: BaseBranch::GithubDefault,
            branch_prefix: DEFAULT_BRANCH_PREFIX.into(),
            auto_fix_ci: true,
            auto_fix_max: 3,
        }
    }
}

impl ForgeSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Parse what a user typed to link a pull request: a URL
/// (`https://host/owner/name/pull/12`, extra path or query allowed),
/// `owner/name#12`, or `#12` / `12` (the thread's own repository).
/// Returns `(host, repo, number)`; `None` host/repo mean "the thread's".
pub fn parse_pr_ref(input: &str) -> Option<(Option<String>, Option<String>, u64)> {
    let s = input.trim();
    let number = |n: &str| n.parse::<u64>().ok().filter(|n| *n > 0);
    let name_ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if let Some(rest) = s.strip_prefix("https://") {
        let rest = rest.split(['?', '#']).next()?;
        let mut parts = rest.split('/');
        let host = parts.next()?.to_ascii_lowercase();
        let owner = parts.next()?;
        let name = parts.next()?;
        if parts.next()? != "pull" {
            return None;
        }
        let n = number(parts.next()?)?;
        let host_ok = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
        if !host_ok || !name_ok(owner) || !name_ok(name) {
            return None;
        }
        return Some((Some(host), Some(format!("{owner}/{name}")), n));
    }
    if let Some((repo, n)) = s.split_once('#') {
        let n = number(n)?;
        if repo.is_empty() {
            return Some((None, None, n));
        }
        let (owner, name) = repo.split_once('/')?;
        if !name_ok(owner) || !name_ok(name) {
            return None;
        }
        return Some((None, Some(repo.to_owned()), n));
    }
    number(s).map(|n| (None, None, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: PrState) -> PrStatus {
        PrStatus {
            state,
            ..PrStatus::default()
        }
    }

    #[test]
    fn badge_priority() {
        assert_eq!(status(PrState::Merged).badge(), PrBadge::Merged);
        assert_eq!(status(PrState::Closed).badge(), PrBadge::Closed);
        let mut s = status(PrState::Open);
        assert_eq!(s.badge(), PrBadge::Open);
        s.mergeable = Mergeable::Mergeable;
        assert_eq!(s.badge(), PrBadge::ReadyToMerge);
        s.review = ReviewDecision::ReviewRequired;
        assert_eq!(s.badge(), PrBadge::Open);
        s.review = ReviewDecision::Approved;
        s.checks.state = ChecksState::Pending;
        assert_eq!(s.badge(), PrBadge::ChecksRunning);
        s.review = ReviewDecision::ChangesRequested;
        assert_eq!(s.badge(), PrBadge::ChangesRequested);
        s.checks.state = ChecksState::Failure;
        assert_eq!(s.badge(), PrBadge::ChecksFailing);
        s.mergeable = Mergeable::Conflicting;
        assert_eq!(s.badge(), PrBadge::Conflict);
        let mut d = status(PrState::Draft);
        d.mergeable = Mergeable::Mergeable;
        assert_eq!(d.badge(), PrBadge::Draft);
    }

    #[test]
    fn parses_pr_references() {
        assert_eq!(
            parse_pr_ref("https://github.com/ts020/blongo/pull/12/files?x=1"),
            Some((Some("github.com".into()), Some("ts020/blongo".into()), 12))
        );
        assert_eq!(
            parse_pr_ref("ts020/blongo#7"),
            Some((None, Some("ts020/blongo".into()), 7))
        );
        assert_eq!(parse_pr_ref("#7"), Some((None, None, 7)));
        assert_eq!(parse_pr_ref(" 7 "), Some((None, None, 7)));
        for bad in [
            "",
            "0",
            "#",
            "x#1",
            "a/b#x",
            "https://github.com/a/b/issues/1",
            "http://github.com/a/b/pull/1",
            "https://github.com/a/../pull/1",
            "https://github.com/a/b/pull/",
        ] {
            assert_eq!(parse_pr_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn settings_default_when_missing() {
        let s: ForgeSettings = serde_json::from_str("{}").unwrap();
        assert!(s.is_default());
        assert!(s.auto_fix_ci);
        let s: ForgeSettings =
            serde_json::from_str(r#"{"base_branch":{"type":"custom","name":"dev"}}"#).unwrap();
        assert_eq!(s.base_branch, BaseBranch::Custom { name: "dev".into() });
        assert_eq!(s.branch_prefix, "blongo/");
    }
}
