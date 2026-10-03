//! Read-mostly workspace operations for the diff panel, the file browser
//! and the branch controls. Every output is bounded: diffs and file lists
//! are read from git's stdout up to a limit and the process is killed past
//! it, so a generated 100 MB file or a monorepo never becomes a 100 MB
//! string.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, bail};
use blongo_protocol::workspace::{
    BranchInfo, DiffFileStat, DiffSummary, DirEntry, FileContent, GitStatusInfo, MAX_DIFF_FILES,
    MAX_READ_BYTES,
};
use tokio::io::AsyncReadExt;

use crate::{INDEX_SEQ, check_untracked_size, git, git_env, head};

/// Most bytes of `git diff` output read for one file.
pub const MAX_FILE_DIFF_BYTES: usize = 8 << 20;
/// Most paths a file listing returns.
pub const MAX_LISTED_FILES: usize = 200_000;
/// Most bytes of a file listing read from git.
const MAX_LIST_BYTES: usize = 32 << 20;
/// Most status entries reported.
const MAX_STATUS_ENTRIES: usize = 500;

/// Run git and read at most `max` bytes of its stdout; past that the
/// process is killed and `truncated` is set.
async fn git_bounded(cwd: &Path, args: &[&str], max: usize) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut child = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .spawn()
        .with_context(|| format!("could not run git {}", args.join(" ")))?;
    let mut stdout = child.stdout.take().context("git stdout")?;
    let mut stderr = child.stderr.take().context("git stderr")?;
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut buf).await;
        buf
    });
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = stdout.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let room = max - out.len();
        if n > room {
            out.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    if truncated {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Ok((out, true));
    }
    let status = child.wait().await?;
    let err = err_task.await.unwrap_or_default();
    if !status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&err).trim()
        );
    }
    Ok((out, false))
}

/// A tree object of the working tree at `cwd` as it is now (tracked and
/// untracked files, ignored ones excluded), written with a throwaway
/// index: nothing the user sees changes and no ref keeps it.
pub async fn snapshot_tree(cwd: &Path) -> anyhow::Result<String> {
    let git_dir = git(cwd, &["rev-parse", "--absolute-git-dir"]).await?;
    let index = PathBuf::from(&git_dir).join(format!(
        "blongo-snapshot-{}-{}.index",
        std::process::id(),
        INDEX_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    check_untracked_size(cwd).await?;
    let result = async {
        let env = [("GIT_INDEX_FILE", index.as_path())];
        match head(cwd).await {
            Some(head) => git_env(cwd, &["read-tree", &head], &env).await?,
            None => git_env(cwd, &["read-tree", "--empty"], &env).await?,
        };
        git_env(cwd, &["add", "-A", "--", "."], &env).await?;
        git_env(cwd, &["write-tree"], &env).await
    }
    .await;
    let _ = std::fs::remove_file(&index);
    result
}

/// Files changed between two tree-ishes, paths relative to `cwd` (changes
/// outside it are left out), at most [`MAX_DIFF_FILES`].
pub async fn diff_summary(cwd: &Path, from: &str, to: &str) -> anyhow::Result<DiffSummary> {
    let base = [
        "diff",
        "--no-color",
        "--no-ext-diff",
        "-M",
        "--relative",
        "-z",
    ];
    let names = [&base[..], &["--name-status", from, to, "--"]].concat();
    let (status_out, status_cut) = git_bounded(cwd, &names, MAX_LIST_BYTES).await?;
    let nums = [&base[..], &["--numstat", from, to, "--"]].concat();
    let (num_out, _) = git_bounded(cwd, &nums, MAX_LIST_BYTES).await?;
    let mut summary = DiffSummary {
        from: from.to_owned(),
        to: to.to_owned(),
        ..DiffSummary::default()
    };
    // --name-status -z: "M\0path\0" or "R100\0old\0new\0".
    let status_text = String::from_utf8_lossy(&status_out);
    let mut fields = status_text.split('\0').filter(|f| !f.is_empty());
    while let Some(code) = fields.next() {
        let status = code.chars().next().unwrap_or('M');
        let (old_path, path) = if matches!(status, 'R' | 'C') {
            let old = fields.next().unwrap_or_default().to_owned();
            (Some(old), fields.next().unwrap_or_default().to_owned())
        } else {
            (None, fields.next().unwrap_or_default().to_owned())
        };
        if summary.files.len() >= MAX_DIFF_FILES {
            summary.truncated = true;
            break;
        }
        summary.files.push(DiffFileStat {
            path,
            old_path,
            status,
            added: 0,
            removed: 0,
            binary: false,
        });
    }
    summary.truncated |= status_cut;
    // --numstat -z: "a\tr\tpath\0" or "a\tr\t\0old\0new\0" ("-" for binary).
    let num_text = String::from_utf8_lossy(&num_out);
    let mut stats: HashMap<String, (Option<u32>, Option<u32>)> = HashMap::new();
    let mut fields = num_text.split('\0');
    while let Some(head) = fields.next() {
        if head.is_empty() {
            continue;
        }
        let mut parts = head.splitn(3, '\t');
        let added = parts.next().and_then(|n| n.parse().ok());
        let removed = parts.next().and_then(|n| n.parse().ok());
        let path = match parts.next() {
            Some("") | None => {
                let _old = fields.next();
                fields.next().unwrap_or_default().to_owned()
            }
            Some(p) => p.to_owned(),
        };
        stats.insert(path, (added, removed));
    }
    for file in &mut summary.files {
        match stats.get(&file.path) {
            Some((Some(a), Some(r))) => {
                file.added = *a;
                file.removed = *r;
            }
            Some(_) => file.binary = true,
            None => {}
        }
        summary.added += file.added as u64;
        summary.removed += file.removed as u64;
    }
    Ok(summary)
}

/// The unified diff of one file between two tree-ishes (at most
/// [`MAX_FILE_DIFF_BYTES`] of it; `truncated` says it was cut).
pub async fn diff_file(
    cwd: &Path,
    from: &str,
    to: &str,
    path: &str,
) -> anyhow::Result<(String, bool)> {
    safe_relative(path)?;
    let (out, cut) = git_bounded(
        cwd,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
            "--relative",
            from,
            to,
            "--",
            path,
        ],
        MAX_FILE_DIFF_BYTES,
    )
    .await?;
    Ok((String::from_utf8_lossy(&out).into_owned(), cut))
}

/// Every file under `cwd` that is not ignored (tracked and untracked), at
/// most [`MAX_LISTED_FILES`]. Outside git: a bounded walk that skips
/// hidden folders and the usual build outputs.
pub async fn list_files(cwd: &Path) -> anyhow::Result<(Vec<String>, bool)> {
    if crate::work_tree_root(cwd).await.is_some() {
        let (out, cut) = git_bounded(
            cwd,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "--deduplicate",
                "-z",
                "--",
                ".",
            ],
            MAX_LIST_BYTES,
        )
        .await?;
        // Splitting and checking up to 200k paths is blocking work: keep
        // it off the caller's (single-threaded) runtime.
        let cwd = cwd.to_path_buf();
        return tokio::task::spawn_blocking(move || {
            let mut files: Vec<String> = String::from_utf8_lossy(&out)
                .split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect();
            let mut truncated = cut;
            if cut {
                files.pop(); // possibly cut in the middle
            }
            if files.len() > MAX_LISTED_FILES {
                files.truncate(MAX_LISTED_FILES);
                truncated = true;
            }
            // Deleted but still tracked files are listed by --cached.
            files.retain(|f| cwd.join(f).symlink_metadata().is_ok());
            (files, truncated)
        })
        .await
        .context("file list");
    }
    let root = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || walk(&root))
        .await
        .context("file walk")
}

fn walk(root: &Path) -> (Vec<String>, bool) {
    const SKIP: [&str; 6] = [
        "node_modules",
        "target",
        "dist",
        "build",
        "__pycache__",
        "venv",
    ];
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = rel.join(&name);
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(_) => {
                    if out.len() >= MAX_LISTED_FILES {
                        return (out, true);
                    }
                    out.push(path.to_string_lossy().into_owned());
                }
                Err(_) => {}
            }
        }
    }
    (out, false)
}

/// Reject absolute paths and `..`: workspace paths stay inside it.
pub fn safe_relative(path: &str) -> anyhow::Result<&Path> {
    let p = Path::new(path);
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        bail!("{path} is not a path inside the workspace");
    }
    Ok(p)
}

/// `path` (relative) resolved under `root`, refused when a symlink leads
/// it outside.
fn resolve_inside(root: &Path, path: &str) -> anyhow::Result<PathBuf> {
    let rel = safe_relative(path)?;
    let root = root.canonicalize()?;
    let full = root.join(rel).canonicalize()?;
    if !full.starts_with(&root) {
        bail!("{path} leads outside the workspace");
    }
    Ok(full)
}

/// Entries of `path` (relative to `cwd`), folders first, ignored entries
/// and `.git` left out.
pub async fn list_dir(cwd: &Path, path: &str) -> anyhow::Result<Vec<DirEntry>> {
    let dir = if path.is_empty() {
        cwd.canonicalize()?
    } else {
        resolve_inside(cwd, path)?
    };
    let mut entries: Vec<DirEntry> = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<DirEntry>> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(&dir)?.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                out.push(DirEntry { name, is_dir });
                if out.len() >= 20_000 {
                    break;
                }
            }
            Ok(out)
        })
        .await??
    };
    if crate::work_tree_root(cwd).await.is_some() && !entries.is_empty() {
        let ignored = check_ignore(&dir, entries.iter().map(|e| e.name.as_str())).await;
        entries.retain(|e| !ignored.contains(&e.name));
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// Names among `names` (entries of `dir`) that git ignores.
async fn check_ignore<'a>(
    dir: &Path,
    names: impl Iterator<Item = &'a str>,
) -> std::collections::HashSet<String> {
    use tokio::io::AsyncWriteExt;
    let mut input = Vec::new();
    for n in names {
        input.extend_from_slice(n.as_bytes());
        input.push(0);
    }
    let child = tokio::process::Command::new("git")
        .args(["check-ignore", "-z", "--stdin"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .spawn();
    let Ok(mut child) = child else {
        return Default::default();
    };
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    let Ok(out) = child.wait_with_output().await else {
        return Default::default();
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// At most `max_bytes` of a file inside `cwd`.
pub async fn read_file(cwd: &Path, path: &str, max_bytes: u32) -> anyhow::Result<FileContent> {
    let full = resolve_inside(cwd, path)?;
    let max = max_bytes.min(MAX_READ_BYTES) as u64;
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<FileContent> {
        use std::io::Read;
        let file = std::fs::File::open(&full)?;
        let len = file.metadata()?.len();
        let mut buf = Vec::new();
        file.take(max).read_to_end(&mut buf)?;
        let binary = buf.iter().take(8000).any(|b| *b == 0);
        let mut text = if binary {
            String::new()
        } else {
            String::from_utf8_lossy(&buf).into_owned()
        };
        if !binary && len > max {
            // Do not end in the middle of a line.
            if let Some(cut) = text.rfind('\n') {
                text.truncate(cut + 1);
            }
        }
        Ok(FileContent {
            path,
            text,
            truncated: len > max,
            binary,
        })
    })
    .await?
}

/// Branch, upstream and changed files of the repository at `cwd`.
pub async fn status(cwd: &Path) -> anyhow::Result<GitStatusInfo> {
    let (out, _) = git_bounded(
        cwd,
        &["status", "--porcelain", "-b", "-z", "--untracked-files=all"],
        MAX_LIST_BYTES,
    )
    .await?;
    Ok(parse_status(&String::from_utf8_lossy(&out)))
}

pub(crate) fn parse_status(text: &str) -> GitStatusInfo {
    let mut info = GitStatusInfo::default();
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        if let Some(head) = field.strip_prefix("## ") {
            let (names, counts) = match head.split_once(" [") {
                Some((n, c)) => (n, Some(c.trim_end_matches(']'))),
                None => (head, None),
            };
            let (branch, upstream) = match names.split_once("...") {
                Some((b, u)) => (b, Some(u)),
                None => (names, None),
            };
            info.branch = if branch.starts_with("HEAD (no branch)") {
                None
            } else {
                Some(branch.trim_start_matches("No commits yet on ").to_owned())
            };
            info.upstream = upstream.map(str::to_owned);
            for part in counts.unwrap_or_default().split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
            continue;
        }
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(2);
        if code.starts_with('R') || code.starts_with('C') {
            let _origin = fields.next();
        }
        if info.changes.len() < MAX_STATUS_ENTRIES {
            info.changes.push((code.to_owned(), path[1..].to_owned()));
        }
    }
    info
}

pub async fn branches(cwd: &Path) -> anyhow::Result<Vec<BranchInfo>> {
    let out = git(
        cwd,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--count=500",
            "--format=%(HEAD)%(refname:short)",
            "refs/heads",
        ],
    )
    .await?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| BranchInfo {
            current: l.starts_with('*'),
            name: l[1..].to_owned(),
        })
        .collect())
}

/// `git switch [-c] branch` (the name checked by git first; it never
/// starts with `-`).
pub async fn switch(cwd: &Path, branch: &str, create: bool) -> anyhow::Result<()> {
    let branch = branch.trim();
    if branch.is_empty() || branch.starts_with('-') {
        bail!("not a branch name: {branch:?}");
    }
    git(cwd, &["check-ref-format", "--branch", branch])
        .await
        .map_err(|_| anyhow::anyhow!("not a valid branch name: {branch}"))?;
    if create {
        git(cwd, &["switch", "--quiet", "-c", branch]).await?;
    } else {
        git(cwd, &["switch", "--quiet", branch]).await?;
    }
    Ok(())
}

/// Stage everything under `cwd` and commit it with the user's own
/// identity and hooks. Returns the new commit's short id.
pub async fn commit_all(cwd: &Path, message: &str) -> anyhow::Result<String> {
    let message = message.trim();
    if message.is_empty() {
        bail!("enter a commit message");
    }
    git(cwd, &["add", "-A", "--", "."]).await?;
    git(cwd, &["commit", "--quiet", "-m", message]).await?;
    git(cwd, &["rev-parse", "--short", "HEAD"]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blongo-git-ws-{name}-{}",
            blongo_protocol::ThreadId::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "--quiet", "-b", "main"]);
        run(&dir, &["config", "user.name", "T"]);
        run(&dir, &["config", "user.email", "t@localhost"]);
        run(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("keep.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.join("old.txt"), "moved content\n".repeat(20)).unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "--quiet", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn snapshot_diff_with_renames_and_bounds() {
        let dir = repo("diff");
        let before = snapshot_tree(&dir).await.unwrap();
        std::fs::write(dir.join("keep.txt"), "a\nB\nc\n").unwrap();
        std::fs::rename(dir.join("old.txt"), dir.join("new.txt")).unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 0, 3]).unwrap();
        std::fs::write(dir.join("noise.log"), "ignored").unwrap();
        let after = snapshot_tree(&dir).await.unwrap();
        // Nothing the user sees changed: the index still has the old state.
        let status = status(&dir).await.unwrap();
        assert!(
            status
                .changes
                .iter()
                .any(|(c, p)| c == " M" && p == "keep.txt")
        );
        let summary = diff_summary(&dir, &before, &after).await.unwrap();
        let by_path: HashMap<&str, &DiffFileStat> =
            summary.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by_path["keep.txt"].status, 'M');
        assert_eq!(
            (by_path["keep.txt"].added, by_path["keep.txt"].removed),
            (1, 1)
        );
        assert_eq!(by_path["new.txt"].status, 'R');
        assert_eq!(by_path["new.txt"].old_path.as_deref(), Some("old.txt"));
        assert!(by_path["bin.dat"].binary);
        assert!(!by_path.contains_key("noise.log"));
        let (text, cut) = diff_file(&dir, &before, &after, "keep.txt").await.unwrap();
        assert!(!cut);
        assert!(text.contains("-b\n+B"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn listings_respect_gitignore_and_stay_inside() {
        let dir = repo("list");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("debug.log"), "").unwrap();
        let (files, cut) = list_files(&dir).await.unwrap();
        assert!(!cut);
        assert!(files.contains(&"src/lib.rs".to_owned()));
        assert!(!files.iter().any(|f| f.ends_with(".log")));
        let top = list_dir(&dir, "").await.unwrap();
        assert_eq!(top[0].name, "src", "folders first: {top:?}");
        assert!(
            !top.iter()
                .any(|e| e.name == "debug.log" || e.name == ".git")
        );
        assert!(safe_relative("../x").is_err());
        assert!(safe_relative("/etc/passwd").is_err());
        assert!(safe_relative("src/./lib.rs").is_ok());
        // A symlink out of the workspace is refused.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", dir.join("out")).unwrap();
            assert!(read_file(&dir, "out/hostname", 100).await.is_err());
        }
        let content = read_file(&dir, "keep.txt", 3).await.unwrap();
        assert!(content.truncated);
        assert_eq!(content.text, "a\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn porcelain_status_is_parsed() {
        let info = parse_status(
            "## main...origin/main [ahead 2, behind 1]\0 M a.rs\0R  new.rs\0old.rs\0?? x\0",
        );
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.upstream.as_deref(), Some("origin/main"));
        assert_eq!((info.ahead, info.behind), (2, 1));
        assert_eq!(
            info.changes,
            [
                (" M".to_owned(), "a.rs".to_owned()),
                ("R ".to_owned(), "new.rs".to_owned()),
                ("??".to_owned(), "x".to_owned())
            ]
        );
        assert_eq!(parse_status("## HEAD (no branch)\0").branch, None);
    }
}
