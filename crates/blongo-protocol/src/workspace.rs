//! Workspace queries: diffs, files, git. Asked with an id
//! (`Backend::query`), answered once with [`crate::client::CoreEvent::Reply`].
//!
//! Every answer is bounded so a huge repository or diff can never become a
//! huge message: summaries list at most [`MAX_DIFF_FILES`] files, a file's
//! hunks stop at the caller's `max_lines`, searches return the best
//! `limit` matches, file reads stop at `max_bytes`.

use serde::{Deserialize, Serialize};

use crate::{ProjectId, ProviderKind, RunId, ThreadId};

/// Client-chosen id that pairs a query with its reply.
pub type QueryId = u64;

/// Most files a diff summary lists.
pub const MAX_DIFF_FILES: usize = 2000;
/// Most lines a single file diff returns, whatever the caller asks.
pub const MAX_DIFF_LINES: u32 = 20_000;
/// Longest diff line kept (longer ones are cut, marked with `…`).
pub const MAX_DIFF_LINE_BYTES: usize = 2000;
/// Most bytes a file read returns.
pub const MAX_READ_BYTES: u32 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Query {
    /// Files changed by one turn (its checkpoint → the next turn's, or the
    /// working tree for the latest turn) or by the whole thread (its first
    /// checkpoint → the working tree).
    DiffSummary {
        thread_id: ThreadId,
        scope: DiffScope,
    },
    /// One file's hunks between two trees from a [`DiffSummary`].
    DiffFile {
        thread_id: ThreadId,
        from: String,
        to: String,
        path: String,
        max_lines: u32,
    },
    /// Fuzzy file search in the thread's folder (respects .gitignore).
    SearchFiles {
        thread_id: ThreadId,
        pattern: String,
        limit: u32,
    },
    /// Entries of one folder (relative to the thread's folder; "" is the
    /// top), ignored files left out.
    ListDir {
        thread_id: ThreadId,
        path: String,
    },
    ReadFile {
        thread_id: ThreadId,
        path: String,
        max_bytes: u32,
    },
    GitStatus {
        thread_id: ThreadId,
    },
    GitBranches {
        thread_id: ThreadId,
    },
    /// Switch the thread's folder to `branch` (`create`: a new branch from
    /// HEAD). Refused while the thread (or one in the same folder) runs.
    GitSwitch {
        thread_id: ThreadId,
        branch: String,
        create: bool,
    },
    /// Commit every change in the thread's folder (`git add -A`).
    GitCommit {
        thread_id: ThreadId,
        message: String,
    },
    /// Check the thread's pull request on GitHub now (and look for the
    /// branch's pull request when none is linked). Answered with `Done`
    /// once the check finished; the status arrives as an event.
    PrRefresh {
        thread_id: ThreadId,
    },
    /// Everything the PR tab shows (fetched now, not stored). Also
    /// refreshes the thread's status.
    PrDetail {
        thread_id: ThreadId,
    },
    /// Change the linked pull request's title and/or body on GitHub.
    /// Refused for read-only links.
    PrEdit {
        thread_id: ThreadId,
        title: Option<String>,
        body: Option<String>,
    },
    /// What the Create PR form starts from (no changes made).
    PrPrepare {
        thread_id: ThreadId,
    },
    /// Send `prompt` to the thread's agent (queued behind a running turn)
    /// and answer with the draft it returns as JSON when the turn ends.
    PrDraft {
        thread_id: ThreadId,
        prompt: String,
    },
    /// Commit (when asked), rename the branch (when never pushed), push
    /// it (never forced) and open the pull request, then link it. Each
    /// step is skipped when already done, so a failed one can be retried.
    PrCreate {
        thread_id: ThreadId,
        request: crate::forge::PrCreateRequest,
    },
    /// Push the thread's branch (never forced).
    PrPush {
        thread_id: ThreadId,
    },
    /// Send the failing checks of the linked pull request to the thread's
    /// agent. Its changes are not pushed for it (the PR tab's Push does);
    /// automatic fixes count from zero again.
    PrFix {
        thread_id: ThreadId,
    },
    /// Send the pull request's unresolved review threads to the thread's
    /// agent (only those in `threads`, by id, when it is not empty).
    PrComments {
        thread_id: ThreadId,
        #[serde(default)]
        threads: Vec<String>,
    },
    /// Fetch the base branch and merge it into the thread's branch
    /// (never a rebase). Conflicts are left in progress and sent to the
    /// thread's agent to resolve; nothing is pushed.
    PrMergeBase {
        thread_id: ThreadId,
    },
    /// Merge the linked pull request on GitHub with `method`, only if its
    /// head is still `sha` (what the user saw). `auto`: enable auto-merge
    /// instead (GitHub merges once the requirements pass).
    PrMerge {
        thread_id: ThreadId,
        method: crate::forge::MergeMethod,
        sha: String,
        #[serde(default)]
        auto: bool,
    },
    /// Archive the thread of a merged pull request; with `delete_remote`,
    /// first delete its branch on GitHub (only if it is still at the
    /// merged commit).
    PrArchive {
        thread_id: ThreadId,
        #[serde(default)]
        delete_remote: bool,
    },
    /// The pull requests, issues and branches a thread of the project
    /// can start from.
    ForgeCandidates {
        project_id: ProjectId,
    },
    /// Start thread `thread_id` in its own worktree on `source`'s work
    /// (`project_id: None`: the project whose remote is the pull request
    /// URL's repository). Answered with `ThreadOpened` once it exists.
    ThreadFrom {
        project_id: Option<ProjectId>,
        thread_id: ThreadId,
        source: crate::forge::ThreadSource,
        #[serde(default)]
        provider: ProviderKind,
        #[serde(default)]
        model: Option<String>,
    },
}

impl Query {
    /// The existing thread the query is about (`None`: a project's).
    pub fn thread_id(&self) -> Option<ThreadId> {
        match self {
            Self::DiffSummary { thread_id, .. }
            | Self::DiffFile { thread_id, .. }
            | Self::SearchFiles { thread_id, .. }
            | Self::ListDir { thread_id, .. }
            | Self::ReadFile { thread_id, .. }
            | Self::GitStatus { thread_id }
            | Self::GitBranches { thread_id }
            | Self::GitSwitch { thread_id, .. }
            | Self::GitCommit { thread_id, .. }
            | Self::PrRefresh { thread_id }
            | Self::PrDetail { thread_id }
            | Self::PrEdit { thread_id, .. }
            | Self::PrPrepare { thread_id }
            | Self::PrDraft { thread_id, .. }
            | Self::PrCreate { thread_id, .. }
            | Self::PrPush { thread_id }
            | Self::PrFix { thread_id }
            | Self::PrComments { thread_id, .. }
            | Self::PrMergeBase { thread_id }
            | Self::PrMerge { thread_id, .. }
            | Self::PrArchive { thread_id, .. } => Some(*thread_id),
            Self::ForgeCandidates { .. } | Self::ThreadFrom { .. } => None,
        }
    }

    /// Changes the workspace (sequenced with the thread's other work).
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::GitSwitch { .. }
                | Self::GitCommit { .. }
                | Self::PrCreate { .. }
                | Self::PrPush { .. }
                | Self::PrMergeBase { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiffScope {
    Turn { run_id: RunId },
    Thread,
}

/// Externally tagged: several variants wrap sequences, which an
/// internally tagged enum cannot carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryReply {
    DiffSummary(DiffSummary),
    DiffFile(FileDiff),
    Files(Vec<FileMatch>),
    Dir(Vec<DirEntry>),
    File(FileContent),
    GitStatus(GitStatusInfo),
    Branches(Vec<BranchInfo>),
    /// A mutation finished; a human-readable summary.
    Done(String),
    PrDetail(Box<crate::forge::PrDetail>),
    PrPrepare(Box<crate::forge::PrPrepare>),
    PrDraft(crate::forge::PrDraft),
    Candidates(Box<crate::forge::ForgeCandidates>),
    ThreadOpened(crate::forge::ThreadOpened),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Tree-ish the diff starts from (a checkpoint commit).
    pub from: String,
    /// Tree-ish it ends at (a later checkpoint, or a snapshot tree of the
    /// working tree taken for this query).
    pub to: String,
    pub files: Vec<DiffFileStat>,
    /// More files changed than [`MAX_DIFF_FILES`].
    pub truncated: bool,
    pub added: u64,
    pub removed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFileStat {
    pub path: String,
    /// Renames and copies: where it came from.
    pub old_path: Option<String>,
    /// `A`dded, `M`odified, `D`eleted, `R`enamed, `C`opied, `T`ype change.
    pub status: char,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// "\ No newline at end of file" and similar.
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// The `@@ -a,b +c,d @@ context` line.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub hunks: Vec<DiffHunk>,
    pub binary: bool,
    /// Lines past `max_lines` were left out.
    pub truncated: bool,
}

impl FileDiff {
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|h| 1 + h.lines.len()).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatch {
    pub path: String,
    pub score: i32,
    /// Byte offsets of the matched characters in `path`.
    pub positions: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub text: String,
    pub truncated: bool,
    pub binary: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusInfo {
    /// `None`: detached HEAD.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// (two-letter porcelain status, path), at most 500.
    pub changes: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub current: bool,
}

/// Parse `git diff` output for one file into hunks, keeping at most
/// `max_lines` lines (hunk headers count as one line each).
pub fn parse_unified_diff(path: &str, text: &str, max_lines: u32) -> FileDiff {
    let max_lines = max_lines.min(MAX_DIFF_LINES) as usize;
    let mut diff = FileDiff {
        path: path.to_owned(),
        ..FileDiff::default()
    };
    let mut lines_kept = 0usize;
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in text.split('\n') {
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            diff.binary = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            if lines_kept >= max_lines {
                diff.truncated = true;
                break;
            }
            let (o, n) = parse_hunk_header(rest);
            old = o;
            new = n;
            in_hunk = true;
            lines_kept += 1;
            diff.hunks.push(DiffHunk {
                header: cut(line),
                lines: Vec::new(),
            });
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (kind, body) = match line.as_bytes().first() {
            Some(b' ') => (LineKind::Context, &line[1..]),
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b'\\') => (LineKind::Meta, line),
            // The empty string after the final newline, or a header of a
            // following file: the hunk is over.
            _ => {
                in_hunk = false;
                continue;
            }
        };
        if lines_kept >= max_lines {
            diff.truncated = true;
            break;
        }
        lines_kept += 1;
        let (o, n) = match kind {
            LineKind::Context => {
                old += 1;
                new += 1;
                (Some(old - 1), Some(new - 1))
            }
            LineKind::Added => {
                new += 1;
                (None, Some(new - 1))
            }
            LineKind::Removed => {
                old += 1;
                (Some(old - 1), None)
            }
            LineKind::Meta => (None, None),
        };
        if let Some(hunk) = diff.hunks.last_mut() {
            hunk.lines.push(DiffLine {
                kind,
                old: o,
                new: n,
                text: cut(body),
            });
        }
    }
    diff
}

/// `-a,b +c,d @@ ...` → (a, c).
fn parse_hunk_header(rest: &str) -> (u32, u32) {
    let mut parts = rest.split_whitespace();
    let num = |s: Option<&str>, sign: char| {
        s.and_then(|s| s.strip_prefix(sign))
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    let old = num(parts.next(), '-');
    let new = num(parts.next(), '+');
    (old, new)
}

fn cut(text: &str) -> String {
    if text.len() <= MAX_DIFF_LINE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_DIFF_LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Fuzzy-match `pattern` against `candidate` (case-insensitive
/// subsequence, fzf-v1 style scoring: consecutive runs, word starts and
/// the file name score higher; shorter paths win ties). `None`: no match.
pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<(i32, Vec<u32>)> {
    if pattern.is_empty() {
        return Some((0, Vec::new()));
    }
    let name_start = candidate.rfind('/').map_or(0, |i| i + 1);
    let mut positions = Vec::with_capacity(pattern.len());
    let mut score = 0i32;
    let mut chars = candidate.char_indices().peekable();
    let mut prev_matched: Option<usize> = None;
    let mut prev_char: Option<char> = None;
    for p in pattern.chars().filter(|c| !c.is_whitespace()) {
        let p = p.to_ascii_lowercase();
        loop {
            let (i, c) = chars.next()?;
            let boundary = matches!(prev_char, None | Some('/' | '_' | '-' | '.' | ' '))
                || (prev_char.is_some_and(|q| q.is_lowercase()) && c.is_uppercase());
            prev_char = Some(c);
            if c.to_ascii_lowercase() == p {
                let mut s = 1;
                if prev_matched.is_some_and(|m| m + 1 == i || candidate[m..i].chars().count() == 1)
                {
                    s += 6;
                }
                if boundary {
                    s += 4;
                }
                if i >= name_start {
                    s += 2;
                }
                score += s;
                positions.push(i as u32);
                prev_matched = Some(i);
                break;
            }
        }
    }
    score -= (candidate.len() / 16) as i32;
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,4 @@ fn main\n fn a() {}\n-fn b() {}\n+fn b() { 1 }\n+fn c() {}\n fn d() {}\n@@ -10,2 +11,2 @@\n-x\n+y\n\\ No newline at end of file\n";

    #[test]
    fn hunks_and_line_numbers() {
        let d = parse_unified_diff("src/a.rs", DIFF, 1000);
        assert_eq!(d.hunks.len(), 2);
        let h = &d.hunks[0];
        assert!(h.header.starts_with("@@ -1,3 +1,4 @@"));
        assert_eq!(h.lines.len(), 5);
        assert_eq!((h.lines[0].old, h.lines[0].new), (Some(1), Some(1)));
        assert_eq!(h.lines[1].kind, LineKind::Removed);
        assert_eq!((h.lines[1].old, h.lines[1].new), (Some(2), None));
        assert_eq!((h.lines[2].old, h.lines[2].new), (None, Some(2)));
        assert_eq!((h.lines[4].old, h.lines[4].new), (Some(3), Some(4)));
        let h = &d.hunks[1];
        assert_eq!((h.lines[0].old, h.lines[1].new), (Some(10), Some(11)));
        assert_eq!(h.lines[2].kind, LineKind::Meta);
        assert!(!d.truncated);
        assert_eq!(d.line_count(), 2 + 5 + 3);
    }

    #[test]
    fn caps_lines_and_long_lines() {
        let d = parse_unified_diff("a", DIFF, 4);
        assert!(d.truncated);
        assert_eq!(d.line_count(), 4);
        let long = format!("@@ -1 +1 @@\n+{}\n", "é".repeat(5000));
        let d = parse_unified_diff("a", &long, 10);
        let text = &d.hunks[0].lines[0].text;
        assert!(text.len() <= MAX_DIFF_LINE_BYTES + 3 && text.ends_with('…'));
        let bin = parse_unified_diff("a", "Binary files a/x and b/x differ\n", 10);
        assert!(bin.binary && bin.hunks.is_empty());
    }

    #[test]
    fn fuzzy_prefers_names_and_runs() {
        assert!(fuzzy_score("xyz", "src/main.rs").is_none());
        let (s1, p) = fuzzy_score("main", "src/main.rs").unwrap();
        assert_eq!(p, vec![4, 5, 6, 7]);
        let (s2, _) = fuzzy_score("main", "src/m/a/i/n.rs").unwrap();
        assert!(s1 > s2);
        let (s3, _) = fuzzy_score("sr", "src/x.rs").unwrap();
        let (s4, _) = fuzzy_score("sr", "docs/very/long/path/xsr.md").unwrap();
        assert!(s3 > s4, "{s3} {s4}");
        assert!(fuzzy_score("MAIN", "src/main.rs").is_some());
        assert_eq!(fuzzy_score("", "a"), Some((0, vec![])));
    }
}
