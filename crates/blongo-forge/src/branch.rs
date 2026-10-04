//! Git work for creating a pull request: the base branch, what the
//! thread's branch holds against it, renaming and pushing it.
//!
//! Pushes are never forced: a remote branch with commits the local one
//! lacks is reported, not overwritten. Credentials are git's own (the
//! user's credential helper or SSH agent); no token passes through here.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Longest a fetch or push may take.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
/// Most commit subjects and uncommitted paths listed.
const MAX_LIST: usize = 200;
/// Longest diff stat kept.
const MAX_STAT: usize = 6_000;
/// Longest pull request template read.
const MAX_TEMPLATE: u64 = 16 * 1024;

/// Fetch `branch` from `remote` into its remote-tracking branch.
pub async fn fetch(cwd: &Path, remote: &str, branch: &str) -> Result<(), String> {
    if !plain(remote) || !plain(branch) {
        return Err("not a branch name".into());
    }
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    run(
        cwd,
        &["fetch", "--quiet", "--no-tags", "--", remote, &refspec],
        NETWORK_TIMEOUT,
    )
    .await
    .map(drop)
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
        NETWORK_TIMEOUT,
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
    let mut fields = out.split('\0').filter(|f| f.len() > 3);
    while let Some(f) = fields.next() {
        if files.len() < MAX_LIST {
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
        fetch(&work, "origin", "blongo/fix-b").await.unwrap();
        assert_eq!(
            commits_since(&work, "origin/blongo/fix-b").await,
            vec!["ours"]
        );
        assert!(fetch(&work, "origin", "-x").await.is_err());

        std::fs::create_dir_all(work.join(".github")).unwrap();
        std::fs::write(work.join(".github/pull_request_template.md"), "## Why").unwrap();
        assert_eq!(template(&work).await.as_deref(), Some("## Why"));
        for d in [remote, work, other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
