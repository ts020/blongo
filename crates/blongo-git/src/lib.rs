//! Git plumbing for checkpoints and worktrees, through the `git` CLI
//! (async `tokio::process`; no libgit2).
//!
//! **Checkpoints** (t3code apps/server/src/vcs/GitVcsDriver.ts): a commit of
//! the whole working tree, tracked and untracked files alike (ignored files
//! excluded), made with a throwaway index so the user's index and HEAD are
//! untouched:
//!
//! ```text
//! GIT_INDEX_FILE=<git-dir>/blongo-checkpoint-<n>.index
//!   git read-tree HEAD          (or --empty in a repo without commits)
//!   git add -A -- .
//!   git write-tree              -> tree
//!   git commit-tree tree [-p HEAD] -m "blongo checkpoint"
//!   git update-ref refs/blongo/checkpoints/<thread>/<run> <commit>
//! ```
//!
//! The user's own index is saved too: its tree (`git write-tree` on the real
//! index, skipped while it has conflicts) becomes a commit "blongo index"
//! that is the checkpoint's second parent.
//!
//! Untracked files larger than [`MAX_UNTRACKED_FILE`] (or more than
//! [`MAX_UNTRACKED_TOTAL`] of them together) make the capture fail instead
//! of copying them into the object store.
//!
//! The hidden ref keeps the commit alive (out of `git log`, branches and
//! pushes). **Restore** puts the working tree back exactly:
//! `git restore --source <commit> --worktree --staged -- .` (when the
//! checkpoint has files), then `git clean -fd -- .` removes files created
//! since (ignored files are left alone), then the index is put back as it
//! was (`git restore --source <commit>^2 --staged -- .`), or re-synced with
//! HEAD (`git reset --quiet -- .`) for a checkpoint without a saved index.
//!
//! **Worktrees**: `git worktree add -b <branch> <path> HEAD`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, bail};

/// Namespace of checkpoint refs.
pub const CHECKPOINT_REF_PREFIX: &str = "refs/blongo/checkpoints";

/// Largest untracked file a checkpoint copies.
pub const MAX_UNTRACKED_FILE: u64 = 64 << 20;
/// Most untracked bytes a checkpoint copies in all.
pub const MAX_UNTRACKED_TOTAL: u64 = 256 << 20;

const INDEX_MESSAGE: &str = "blongo index";

/// Namespace of the refs keeping the files a rollback replaced.
pub const PRE_ROLLBACK_REF_PREFIX: &str = "refs/blongo/pre-rollback";

static INDEX_SEQ: AtomicU64 = AtomicU64::new(0);

async fn git_env(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> anyhow::Result<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Never prompt, never pick up a pager or the user's hooks editor.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    for (k, v) in env {
        command.env(k, v);
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("could not run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    git_env(cwd, args, &[]).await
}

/// The top of the work tree containing `path`, or `None` outside git.
pub async fn work_tree_root(path: &Path) -> Option<PathBuf> {
    git(path, &["rev-parse", "--show-toplevel"])
        .await
        .ok()
        .map(PathBuf::from)
}

async fn head(cwd: &Path) -> Option<String> {
    git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .await
        .ok()
        .filter(|h| !h.is_empty())
}

/// The ref a run's checkpoint lives under.
pub fn checkpoint_ref(thread: &str, run: &str) -> String {
    format!("{CHECKPOINT_REF_PREFIX}/{thread}/{run}")
}

/// Capture the working tree of the repository at `cwd` into a commit kept
/// alive by `ref_name`. Returns the commit id.
pub async fn capture_checkpoint(cwd: &Path, ref_name: &str) -> anyhow::Result<String> {
    let git_dir = git(cwd, &["rev-parse", "--absolute-git-dir"]).await?;
    let index = PathBuf::from(&git_dir).join(format!(
        "blongo-checkpoint-{}-{}.index",
        std::process::id(),
        INDEX_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    check_untracked_size(cwd).await?;
    let result = async {
        let env = [("GIT_INDEX_FILE", index.as_path())];
        let head = head(cwd).await;
        // The user's index as it is (fails while it has conflicts: then
        // there is no clean tree to save and restore re-syncs with HEAD).
        // Read from a copy: `write-tree` on the real index would take
        // index.lock and rewrite it.
        let real_index = git(cwd, &["rev-parse", "--git-path", "index"]).await?;
        let real_index = cwd.join(real_index);
        let user_index = index.with_extension("user-index");
        if real_index.is_file() {
            std::fs::copy(&real_index, &user_index)?;
        }
        let written = git_env(
            cwd,
            &["write-tree"],
            &[("GIT_INDEX_FILE", user_index.as_path())],
        )
        .await;
        let _ = std::fs::remove_file(&user_index);
        let saved_index = match written {
            Ok(tree) => {
                let mut args = vec!["commit-tree", tree.as_str()];
                if let Some(head) = &head {
                    args.extend(["-p", head.as_str()]);
                }
                args.extend(["-m", INDEX_MESSAGE]);
                Some(commit_tree(cwd, &args, &[]).await?)
            }
            Err(_) => None,
        };
        match &head {
            Some(head) => git_env(cwd, &["read-tree", head], &env).await?,
            None => git_env(cwd, &["read-tree", "--empty"], &env).await?,
        };
        git_env(cwd, &["add", "-A", "--", "."], &env).await?;
        let tree = git_env(cwd, &["write-tree"], &env).await?;
        let mut args = vec!["commit-tree", tree.as_str()];
        match (&head, &saved_index) {
            (Some(head), Some(index)) => args.extend(["-p", head.as_str(), "-p", index.as_str()]),
            (Some(head), None) => args.extend(["-p", head.as_str()]),
            // No commits yet: the index commit is the only parent.
            (None, Some(index)) => args.extend(["-p", index.as_str()]),
            (None, None) => {}
        }
        args.extend(["-m", "blongo checkpoint"]);
        let commit = commit_tree(cwd, &args, &env).await?;
        git(cwd, &["update-ref", ref_name, &commit]).await?;
        Ok(commit)
    }
    .await;
    let _ = std::fs::remove_file(&index);
    result
}

/// `git commit-tree` with a fixed identity: a checkpoint is not the user's
/// commit.
async fn commit_tree(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> anyhow::Result<String> {
    let identity = [
        "-c",
        "user.name=Blongo",
        "-c",
        "user.email=blongo@localhost",
    ];
    git_env(cwd, &[&identity[..], args].concat(), env).await
}

/// Refuse to copy huge untracked files (build outputs nobody ignored,
/// downloads) into the object store.
async fn check_untracked_size(cwd: &Path) -> anyhow::Result<()> {
    let list = git(
        cwd,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )
    .await?;
    let mut total = 0u64;
    for name in list.split('\0').filter(|n| !n.is_empty()) {
        let Ok(meta) = std::fs::symlink_metadata(cwd.join(name)) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > MAX_UNTRACKED_FILE {
            bail!(
                "untracked file {name} is {} MiB; checkpoints skip folders with untracked \
                 files over {} MiB (ignore it in .gitignore to allow checkpoints)",
                meta.len() >> 20,
                MAX_UNTRACKED_FILE >> 20
            );
        }
        total += meta.len();
        if total > MAX_UNTRACKED_TOTAL {
            bail!(
                "untracked files add up to more than {} MiB; checkpoints skip such folders",
                MAX_UNTRACKED_TOTAL >> 20
            );
        }
    }
    Ok(())
}

/// The index commit saved with a checkpoint, if any.
async fn saved_index(cwd: &Path, commit: &str) -> Option<String> {
    for parent in [format!("{commit}^2"), format!("{commit}^1")] {
        let Ok(id) = git(cwd, &["rev-parse", "--verify", "--quiet", &parent]).await else {
            continue;
        };
        let message = git(cwd, &["log", "-1", "--format=%s", &id]).await.ok()?;
        if message == INDEX_MESSAGE {
            return Some(id);
        }
    }
    None
}

/// Make the working tree at `cwd` match checkpoint `commit` exactly
/// (ignored files aside). HEAD and branches do not move.
pub async fn restore_checkpoint(cwd: &Path, commit: &str) -> anyhow::Result<()> {
    let tree = format!("{commit}^{{tree}}");
    let has_files = !git(cwd, &["ls-tree", "--name-only", &tree])
        .await?
        .is_empty();
    if has_files {
        git(
            cwd,
            &[
                "restore",
                "--source",
                commit,
                "--worktree",
                "--staged",
                "--",
                ".",
            ],
        )
        .await?;
    }
    // Files created after the checkpoint are untracked now: remove them.
    git(cwd, &["clean", "-fd", "--", "."]).await?;
    if let Some(index) = saved_index(cwd, commit).await {
        // Back to what was staged then (a partially staged file keeps its
        // staged version).
        let has_staged = !git(
            cwd,
            &[
                "ls-tree",
                "--name-only",
                &format!("{index}^{{tree}}"),
                "--",
                ".",
            ],
        )
        .await?
        .is_empty();
        if has_staged {
            git(cwd, &["restore", "--source", &index, "--staged", "--", "."]).await?;
            return Ok(());
        }
    }
    if head(cwd).await.is_some() {
        git(cwd, &["reset", "--quiet", "--", "."]).await?;
    } else {
        // No commits: an empty index is HEAD's state.
        let _ = git(
            cwd,
            &[
                "rm",
                "-r",
                "--cached",
                "--quiet",
                "--ignore-unmatch",
                "--",
                ".",
            ],
        )
        .await;
    }
    Ok(())
}

/// Delete checkpoint refs (best effort).
pub async fn delete_refs(cwd: &Path, refs: &[String]) {
    for r in refs {
        let _ = git(cwd, &["update-ref", "-d", r]).await;
    }
}

/// Delete a thread's checkpoint refs (best effort), except those pointing
/// at a commit in `keep` (still used by a fork's copied runs).
/// Pre-rollback refs are never deleted here: they hold files a rollback
/// replaced, which only the user may discard.
pub async fn delete_thread_refs(cwd: &Path, thread: &str, keep: &HashSet<String>) {
    let Ok(list) = git(
        cwd,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            &format!("{CHECKPOINT_REF_PREFIX}/{thread}/"),
        ],
    )
    .await
    else {
        return;
    };
    let refs: Vec<String> = list
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(commit, _)| !keep.contains(*commit))
        .map(|(_, name)| name.to_owned())
        .collect();
    delete_refs(cwd, &refs).await;
}

/// Create a worktree for `branch` at `path` from the repository at `repo`'s
/// current HEAD.
pub async fn add_worktree(repo: &Path, path: &Path, branch: &str) -> anyhow::Result<()> {
    if head(repo).await.is_none() {
        bail!("the project has no commits yet; a worktree needs one");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_str = path.to_string_lossy();
    git(
        repo,
        &[
            "worktree", "add", "--quiet", "-b", branch, &path_str, "HEAD",
        ],
    )
    .await?;
    Ok(())
}

/// The top of the worktree containing `path`, checked to be one Blongo
/// may delete: a linked worktree (never a main checkout) of `repo`'s
/// repository (same git common dir) whose top lies strictly inside `root`
/// (where Blongo creates worktrees, `<data_dir>/worktrees`). Every path is
/// resolved (symlinks, `..`) before it is compared.
pub async fn owned_worktree_top(repo: &Path, path: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    let top = work_tree_root(path)
        .await
        .ok_or_else(|| anyhow::anyhow!("{} is not in a git work tree", path.display()))?
        .canonicalize()?;
    let root = root.canonicalize()?;
    if top == root || !top.starts_with(&root) {
        bail!(
            "{} is not inside Blongo's worktree folder {}",
            top.display(),
            root.display()
        );
    }
    let dirs = |cwd: PathBuf| async move {
        let out = git(
            &cwd,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        )
        .await?;
        let mut lines = out.lines().map(|l| Path::new(l).canonicalize());
        match (lines.next(), lines.next()) {
            (Some(dir), Some(common)) => Ok::<_, anyhow::Error>((dir?, common?)),
            _ => bail!("unexpected git rev-parse output"),
        }
    };
    let (git_dir, common) = dirs(top.clone()).await?;
    let (_, repo_common) = dirs(repo.to_path_buf()).await?;
    if common != repo_common {
        bail!("{} belongs to another repository", top.display());
    }
    if git_dir == common {
        bail!("{} is a main checkout, not a worktree", top.display());
    }
    Ok(top)
}

/// Remove a worktree created by [`add_worktree`] (its branch stays), after
/// [`owned_worktree_top`] confirmed it is one of ours under `root`.
pub async fn remove_worktree(repo: &Path, path: &Path, root: &Path) -> anyhow::Result<()> {
    let top = owned_worktree_top(repo, path, root).await?;
    git(
        repo,
        &["worktree", "remove", "--force", &top.to_string_lossy()],
    )
    .await?;
    Ok(())
}

/// Remove the worktree containing `path` (only one [`owned_worktree_top`]
/// accepts under `root`; a project inside a larger
/// repository works in a subfolder of it) only when nothing in it would be
/// lost: no uncommitted change, no untracked file, no ignored file (`git
/// worktree remove` alone deletes ignored files such as `.env`) and no
/// modified submodule. The check passes its own flags so no user setting
/// (`status.showUntrackedFiles=no`, `submodule.*.ignore`) can hide a file.
/// Returns why it was kept otherwise; its branch stays either way.
///
/// The check and the removal are two steps: a process outside Blongo (the
/// user's shell or editor) that writes into the folder in between can still
/// lose that write. Blongo itself runs nothing there once the thread is
/// archived.
pub async fn remove_pristine_worktree(repo: &Path, path: &Path, root: &Path) -> anyhow::Result<()> {
    let top = owned_worktree_top(repo, path, root).await?;
    let status = git(
        &top,
        &[
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
    .await?;
    if !status.is_empty() {
        let first = status.lines().next().unwrap_or_default();
        let n = status.lines().count();
        bail!(
            "it has uncommitted, untracked or ignored files ({n} entr{}, e.g. `{first}`)",
            if n == 1 { "y" } else { "ies" }
        );
    }
    git(repo, &["worktree", "remove", &top.to_string_lossy()]).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
