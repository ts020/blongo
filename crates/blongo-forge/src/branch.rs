//! Git work for creating a pull request: the base branch, what the
//! thread's branch holds against it, renaming and pushing it.
//!
//! Pushes are never forced: a remote branch with commits the local one
//! lacks is reported, not overwritten. Credentials are git's own (the
//! user's credential helper or SSH agent); no token passes through here.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Longest a fetch may take by default.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest a push may take (a large first push).
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);
/// Most commit subjects and uncommitted paths listed.
const MAX_LIST: usize = 200;
/// Longest diff stat kept.
const MAX_STAT: usize = 6_000;
/// Longest pull request template read.
const MAX_TEMPLATE: u64 = 16 * 1024;

/// Fetch `branch` from `remote` into its remote-tracking branch, giving
/// up after `timeout` (git is killed then; a stale ref lock it leaves is
/// git's to report on the next fetch).
pub async fn fetch(
    cwd: &Path,
    remote: &str,
    branch: &str,
    timeout: Duration,
) -> Result<(), String> {
    if !plain(remote) || !plain(branch) {
        return Err("not a branch name".into());
    }
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    run(
        cwd,
        &["fetch", "--quiet", "--no-tags", "--", remote, &refspec],
        timeout,
    )
    .await
    .map(drop)
}

/// Fetch pull request `number`'s head (`refs/pull/N/head`, a fork's work
/// too) into the hidden ref it returns, `refs/blongo/pull/N`.
pub async fn fetch_pull(cwd: &Path, remote: &str, number: u64) -> Result<String, String> {
    if !plain(remote) {
        return Err("not a remote name".into());
    }
    let local = format!("refs/blongo/pull/{number}");
    let refspec = format!("+refs/pull/{number}/head:{local}");
    run(
        cwd,
        &["fetch", "--quiet", "--no-tags", "--", remote, &refspec],
        FETCH_TIMEOUT,
    )
    .await?;
    Ok(local)
}

/// How a local branch compares with a commit just fetched for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Same,
    /// It was behind and now points at the fetched commit.
    Forwarded,
    /// It has commits the fetched one lacks (and maybe lacks some).
    Differs,
}

/// Fast-forward the local `branch` (not checked out anywhere) to
/// `target` when it is behind; never moves it otherwise. Only the local
/// ref changes, and only if it still is what was compared.
pub async fn fast_forward(cwd: &Path, branch: &str, target: &str) -> Result<Freshness, String> {
    if !plain(branch) || !plain(target) {
        return Err("not a branch name".into());
    }
    let local = format!("refs/heads/{branch}");
    let old = quick(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{local}^{{commit}}"),
        ],
    )
    .await?;
    let new = quick(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{target}^{{commit}}"),
        ],
    )
    .await?;
    if old == new {
        return Ok(Freshness::Same);
    }
    if quick(cwd, &["merge-base", "--is-ancestor", &old, &new])
        .await
        .is_err()
    {
        return Ok(Freshness::Differs);
    }
    quick(
        cwd,
        &[
            "update-ref",
            "-m",
            "blongo: fast-forward",
            &local,
            &new,
            &old,
        ],
    )
    .await?;
    Ok(Freshness::Forwarded)
}

/// The branches checked out in `cwd`'s repository (its main checkout
/// and every worktree), with where.
pub async fn checked_out(cwd: &Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(list) = quick(cwd, &["worktree", "list", "--porcelain"]).await else {
        return out;
    };
    let mut path = String::new();
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = p.to_owned();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            out.insert(b.to_owned(), path.clone());
        }
    }
    out
}

/// Local branches and `remote`'s branches with no local namesake,
/// newest commit first, at most `limit`; `true`: only on the remote.
pub async fn recent_branches(
    cwd: &Path,
    remote: Option<&str>,
    limit: usize,
) -> Vec<(String, bool)> {
    let remote = remote.filter(|r| plain(r)).unwrap_or("");
    // No remote: a pattern nothing matches.
    let remotes = if remote.is_empty() {
        "refs/heads/.none".to_owned()
    } else {
        format!("refs/remotes/{remote}")
    };
    let Ok(out) = quick(
        cwd,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)",
            "refs/heads",
            &remotes,
        ],
    )
    .await
    else {
        return Vec::new();
    };
    let prefix = format!("{remotes}/");
    let local: std::collections::HashSet<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("refs/heads/"))
        .collect();
    let mut seen = std::collections::HashSet::new();
    out.lines()
        .filter_map(|l| {
            if let Some(b) = l.strip_prefix("refs/heads/") {
                Some((b.to_owned(), false))
            } else {
                let b = l.strip_prefix(&prefix)?;
                (b != "HEAD" && !local.contains(b)).then(|| (b.to_owned(), true))
            }
        })
        .filter(|(b, _)| seen.insert(b.clone()))
        .take(limit)
        .collect()
}

/// The remote-tracking branch `refs/remotes/{remote}/{branch}` exists.
pub async fn has_remote_branch(cwd: &Path, remote: &str, branch: &str) -> bool {
    plain(remote)
        && plain(branch)
        && quick(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{remote}/{branch}^{{commit}}"),
            ],
        )
        .await
        .is_ok()
}

/// The branch `remote`'s HEAD points at, as of the last fetch or clone
/// (`refs/remotes/{remote}/HEAD`).
pub async fn remote_head(cwd: &Path, remote: &str) -> Option<String> {
    if !plain(remote) {
        return None;
    }
    let full = quick(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .await
    .ok()?;
    full.strip_prefix(&format!("{remote}/"))
        .map(str::to_owned)
        .filter(|b| !b.is_empty())
}

/// The commit `remote` has for `branch` now (`None`: no such branch).
/// Asks the remote; no ref changes.
pub async fn remote_branch_tip(
    cwd: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    if !plain(remote) || !plain(branch) {
        return Err("not a branch name".into());
    }
    let out = run(
        cwd,
        &[
            "ls-remote",
            "--heads",
            "--",
            remote,
            &format!("refs/heads/{branch}"),
        ],
        Duration::from_secs(30),
    )
    .await?;
    Ok(out
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .filter(|s| !s.is_empty()))
}

/// The commit HEAD points at.
pub async fn head(cwd: &Path) -> Option<String> {
    quick(cwd, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
        .ok()
}

/// The local branch `refs/heads/{branch}` exists.
pub async fn has_local_branch(cwd: &Path, branch: &str) -> bool {
    plain(branch)
        && quick(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
        .is_ok()
}

/// Rename the local branch `from` to `to` (refused when `to` exists).
pub async fn rename(cwd: &Path, from: &str, to: &str) -> Result<(), String> {
    if !plain(from) || !plain(to) {
        return Err("not a branch name".into());
    }
    if has_local_branch(cwd, to).await {
        return Err(format!("a branch named {to} exists already"));
    }
    quick(cwd, &["branch", "-m", from, to]).await.map(drop)
}

/// Push `branch` to the same name on `remote` and track it there. Never
/// forced: GitHub's branch moving on without this one is an error.
pub async fn push(cwd: &Path, remote: &str, branch: &str) -> Result<(), String> {
    if !plain(remote) || !plain(branch) {
        return Err("not a branch name".into());
    }
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    match run(
        cwd,
        &["push", "--quiet", "--set-upstream", "--", remote, &refspec],
        PUSH_TIMEOUT,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(err)
            if err.contains("[rejected]")
                || err.contains("non-fast-forward")
                || err.contains("fetch first") =>
        {
            Err(format!(
                "{remote}/{branch} has commits this branch lacks; bring them in first (Blongo \
                 never force-pushes)"
            ))
        }
        Err(err) => Err(err),
    }
}

/// Subjects of the commits on HEAD that `base` lacks, oldest first.
pub async fn commits_since(cwd: &Path, base: &str) -> Vec<String> {
    quick(
        cwd,
        &[
            "log",
            "--reverse",
            "--no-merges",
            &format!("--max-count={MAX_LIST}"),
            "--format=%s",
            &format!("{base}..HEAD"),
            "--",
        ],
    )
    .await
    .map(|out| out.lines().map(str::to_owned).collect())
    .unwrap_or_default()
}

/// `git diff --stat` of the working tree against where HEAD left `base`
/// (committed and uncommitted changes of tracked files), cut when long.
pub async fn diff_stat(cwd: &Path, base: &str) -> String {
    let Ok(fork) = quick(cwd, &["merge-base", base, "HEAD"]).await else {
        return String::new();
    };
    let mut stat = quick(cwd, &["diff", "--stat=100", &fork, "--"])
        .await
        .unwrap_or_default();
    if stat.len() > MAX_STAT {
        let mut end = MAX_STAT;
        while !stat.is_char_boundary(end) {
            end -= 1;
        }
        stat.truncate(end);
        stat.push_str("\n…");
    }
    stat
}

/// Paths changed and not committed (untracked ones included), at most
/// [`MAX_LIST`].
pub async fn uncommitted_files(cwd: &Path) -> Vec<String> {
    let Ok(out) = quick(cwd, &["status", "--porcelain", "-z"]).await else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        if files.len() < MAX_LIST && f.len() > 3 {
            files.push(f[3..].to_owned());
        }
        // Renames and copies carry their old path as an extra field.
        if f.starts_with('R') || f.starts_with('C') {
            fields.next();
        }
    }
    files
}

/// The repository's pull request template: the usual file names under
/// `.github/`, `docs/` or the top, first match, at most 16 KiB.
pub async fn template(cwd: &Path) -> Option<String> {
    let top = quick(cwd, &["rev-parse", "--show-toplevel"]).await.ok()?;
    let top = Path::new(&top);
    for dir in [".github", "", "docs"] {
        for name in ["pull_request_template.md", "PULL_REQUEST_TEMPLATE.md"] {
            let path = top.join(dir).join(name);
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            // Only a plain file inside the repository.
            if !meta.is_file() || meta.len() > MAX_TEMPLATE {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                return Some(text);
            }
        }
    }
    None
}

/// What bringing the base branch into the checked-out branch did.
#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The branch has the base's commits already.
    UpToDate,
    /// Merged without conflicts (a merge commit, nothing pushed).
    Merged,
    /// The merge stopped with these files in conflict; it is left in
    /// progress for someone to resolve and commit.
    Conflicts(Vec<String>),
}

/// Merge `refs/remotes/{remote}/{base}` (fetched by the caller) into the
/// checked-out branch. Never a rebase; a merge already in progress is
/// reported with its conflicted files instead of starting another.
pub async fn merge_base(cwd: &Path, remote: &str, base: &str) -> Result<MergeOutcome, String> {
    if !plain(remote) || !plain(base) {
        return Err("not a branch name".into());
    }
    if quick(cwd, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .await
        .is_ok()
    {
        let files = conflicted(cwd).await;
        return if files.is_empty() {
            Err("a merge is in progress; commit it first".into())
        } else {
            Ok(MergeOutcome::Conflicts(files))
        };
    }
    let target = format!("refs/remotes/{remote}/{base}");
    if quick(cwd, &["merge-base", "--is-ancestor", &target, "HEAD"])
        .await
        .is_ok()
    {
        return Ok(MergeOutcome::UpToDate);
    }
    if !uncommitted_files(cwd).await.is_empty() {
        return Err("commit or discard the uncommitted changes first".into());
    }
    let message = format!("Merge {remote}/{base}");
    match run(
        cwd,
        &["merge", "--no-edit", "--no-ff", "-m", &message, &target],
        Duration::from_secs(120),
    )
    .await
    {
        Ok(_) => Ok(MergeOutcome::Merged),
        Err(err) => {
            let files = conflicted(cwd).await;
            if files.is_empty() {
                let _ = quick(cwd, &["merge", "--abort"]).await;
                Err(err)
            } else {
                Ok(MergeOutcome::Conflicts(files))
            }
        }
    }
}

/// A merge is in progress or files are in conflict: nothing here may be
/// committed for the user.
pub async fn unmerged(cwd: &Path) -> bool {
    quick(cwd, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .await
        .is_ok()
        || !conflicted(cwd).await.is_empty()
}

/// Files with unresolved conflicts.
async fn conflicted(cwd: &Path) -> Vec<String> {
    quick(cwd, &["diff", "--name-only", "--diff-filter=U", "-z"])
        .await
        .map(|out| {
            out.split('\0')
                .filter(|f| !f.is_empty())
                .take(MAX_LIST)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Neither empty nor an option, no whitespace or control characters: a
/// name safe to put after the arguments it follows (callers validate
/// branch names more strictly).
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

async fn quick(cwd: &Path, args: &[&str]) -> Result<String, String> {
    run(cwd, args, Duration::from_secs(30)).await
}

async fn run(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let child = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(timeout, child)
        .await
        .map_err(|_| format!("git {} took too long", args[0]))?
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err: String = err.trim().chars().take(600).collect();
        return Err(format!("git {} failed: {err}", args[0]));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(cwd: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blongo-branch-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn merges_the_base_and_reports_conflicts() {
        let work = temp("merge");
        sh(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-qm", "init"]);
        // A fake remote-tracking branch for the base.
        sh(&work, &["checkout", "-qb", "topic"]);
        sh(&work, &["update-ref", "refs/remotes/origin/main", "main"]);
        assert_eq!(
            merge_base(&work, "origin", "main").await,
            Ok(MergeOutcome::UpToDate)
        );
        assert!(!unmerged(&work).await);
        // The base moves on elsewhere: a clean merge.
        sh(&work, &["checkout", "-q", "main"]);
        std::fs::write(work.join("b.txt"), "b\n").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-qm", "b"]);
        sh(&work, &["update-ref", "refs/remotes/origin/main", "main"]);
        sh(&work, &["checkout", "-q", "topic"]);
        sh(&work, &["config", "user.name", "t"]);
        sh(&work, &["config", "user.email", "t@t"]);
        assert_eq!(
            merge_base(&work, "origin", "main").await,
            Ok(MergeOutcome::Merged)
        );
        assert!(work.join("b.txt").exists());
        // Both sides change a.txt: conflicts are left for the agent.
        sh(&work, &["checkout", "-q", "main"]);
        std::fs::write(work.join("a.txt"), "base\n").unwrap();
        sh(&work, &["commit", "-qam", "base"]);
        sh(&work, &["update-ref", "refs/remotes/origin/main", "main"]);
        sh(&work, &["checkout", "-q", "topic"]);
        std::fs::write(work.join("a.txt"), "topic\n").unwrap();
        // Uncommitted changes are refused before merging.
        assert!(merge_base(&work, "origin", "main").await.is_err());
        sh(&work, &["commit", "-qam", "topic"]);
        let want = Ok(MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert_eq!(merge_base(&work, "origin", "main").await, want);
        // Asking again reports the merge in progress.
        assert_eq!(merge_base(&work, "origin", "main").await, want);
        assert!(unmerged(&work).await);
        assert_eq!(
            merge_base(&work, "-x", "main").await,
            Err("not a branch name".into())
        );
    }

    #[tokio::test]
    async fn rename_push_and_never_force() {
        let remote = temp("remote");
        let work = temp("work");
        let other = temp("other");
        sh(&remote, &["init", "-q", "--bare", "-b", "main"]);
        sh(&work, &["init", "-q", "-b", "main"]);
        sh(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        std::fs::write(work.join("a"), "1").unwrap();
        sh(&work, &["add", "a"]);
        sh(&work, &["commit", "-qm", "one"]);
        sh(&work, &["push", "-q", "origin", "main"]);
        sh(&work, &["switch", "-qc", "blongo/abc"]);
        std::fs::write(work.join("b"), "2").unwrap();
        std::fs::write(work.join("c"), "3").unwrap();
        sh(&work, &["add", "b"]);
        sh(&work, &["commit", "-qm", "two"]);

        assert_eq!(uncommitted_files(&work).await, vec!["c".to_owned()]);
        assert_eq!(commits_since(&work, "origin/main").await, vec!["two"]);
        assert!(diff_stat(&work, "origin/main").await.contains("b |"));
        assert!(rename(&work, "blongo/abc", "main").await.is_err());
        rename(&work, "blongo/abc", "blongo/fix-b").await.unwrap();
        assert!(has_local_branch(&work, "blongo/fix-b").await);
        push(&work, "origin", "blongo/fix-b").await.unwrap();
        assert!(has_remote_branch(&work, "origin", "blongo/fix-b").await);

        // Someone else moves the remote branch on: no force.
        sh(&other, &["clone", "-q", remote.to_str().unwrap(), "."]);
        sh(&other, &["switch", "-q", "blongo/fix-b"]);
        std::fs::write(other.join("d"), "4").unwrap();
        sh(&other, &["add", "d"]);
        sh(&other, &["commit", "-qm", "theirs"]);
        sh(&other, &["push", "-q", "origin", "blongo/fix-b"]);
        sh(&work, &["commit", "-qam", "ours", "--allow-empty"]);
        let err = push(&work, "origin", "blongo/fix-b").await.unwrap_err();
        assert!(err.contains("never force-pushes"), "{err}");
        fetch(&work, "origin", "blongo/fix-b", FETCH_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            commits_since(&work, "origin/blongo/fix-b").await,
            vec!["ours"]
        );
        assert!(fetch(&work, "origin", "-x", FETCH_TIMEOUT).await.is_err());
        assert!(
            remote_branch_tip(&work, "origin", "blongo/fix-b")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            remote_branch_tip(&work, "origin", "blongo/nope").await,
            Ok(None)
        );

        std::fs::create_dir_all(work.join(".github")).unwrap();
        std::fs::write(work.join(".github/pull_request_template.md"), "## Why").unwrap();
        assert_eq!(template(&work).await.as_deref(), Some("## Why"));
        for d in [remote, work, other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
