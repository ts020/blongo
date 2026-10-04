//! Workspace query execution (diffs, files, git), off the core loop: the
//! orchestrator resolves the thread's folder and the checkpoints involved,
//! then runs one of these as a task and sends the reply straight to the
//! client.

use std::path::PathBuf;

use blongo_git::workspace as ws;
use blongo_protocol::workspace::{FileMatch, QueryReply, fuzzy_score, parse_unified_diff};

/// What to run, with everything resolved from core state.
pub(crate) enum Plan {
    /// `to: None`: the working tree as it is now.
    DiffSummary {
        from: String,
        to: Option<String>,
    },
    DiffFile {
        from: String,
        to: String,
        path: String,
        max_lines: u32,
    },
    Search {
        pattern: String,
        limit: u32,
    },
    ListDir {
        path: String,
    },
    ReadFile {
        path: String,
        max_bytes: u32,
    },
    Status,
    Branches,
    Switch {
        branch: String,
        create: bool,
    },
    Commit {
        message: String,
    },
}

/// Most matches a search returns.
const MAX_MATCHES: u32 = 200;

pub(crate) async fn run(cwd: PathBuf, plan: Plan) -> Result<QueryReply, String> {
    let err = |e: anyhow::Error| format!("{e:#}");
    match plan {
        Plan::DiffSummary { from, to } => {
            let to = match to {
                Some(to) => to,
                None => ws::snapshot_tree(&cwd).await.map_err(err)?,
            };
            ws::diff_summary(&cwd, &from, &to)
                .await
                .map(QueryReply::DiffSummary)
                .map_err(err)
        }
        Plan::DiffFile {
            from,
            to,
            path,
            max_lines,
        } => {
            let (text, cut) = ws::diff_file(&cwd, &from, &to, &path).await.map_err(err)?;
            // Parsing megabytes is CPU work: keep it off the core thread.
            tokio::task::spawn_blocking(move || {
                let mut diff = parse_unified_diff(&path, &text, max_lines);
                diff.truncated |= cut;
                QueryReply::DiffFile(diff)
            })
            .await
            .map_err(|e| e.to_string())
        }
        Plan::Search { pattern, limit } => {
            let (files, _) = ws::list_files(&cwd).await.map_err(err)?;
            let limit = limit.clamp(1, MAX_MATCHES) as usize;
            tokio::task::spawn_blocking(move || QueryReply::Files(search(&files, &pattern, limit)))
                .await
                .map_err(|e| e.to_string())
        }
        Plan::ListDir { path } => ws::list_dir(&cwd, &path)
            .await
            .map(QueryReply::Dir)
            .map_err(err),
        Plan::ReadFile { path, max_bytes } => ws::read_file(&cwd, &path, max_bytes)
            .await
            .map(QueryReply::File)
            .map_err(err),
        Plan::Status => ws::status(&cwd)
            .await
            .map(QueryReply::GitStatus)
            .map_err(err),
        Plan::Branches => ws::branches(&cwd)
            .await
            .map(QueryReply::Branches)
            .map_err(err),
        Plan::Switch { branch, create } => {
            ws::switch(&cwd, &branch, create).await.map_err(err)?;
            Ok(QueryReply::Done(if create {
                format!("Created and switched to {branch}")
            } else {
                format!("Switched to {branch}")
            }))
        }
        Plan::Commit { message } => {
            let id = ws::commit_all(&cwd, &message).await.map_err(err)?;
            Ok(QueryReply::Done(format!("Committed {id}")))
        }
    }
}

/// The best `limit` fuzzy matches of `pattern` among `files` (an empty
/// pattern lists the first files).
pub(crate) fn search(files: &[String], pattern: &str, limit: usize) -> Vec<FileMatch> {
    let pattern = pattern.trim();
    // A bounded min-heap would save little here: score into a vector of
    // (score, index) only for matches, then keep the top ones.
    let mut hits: Vec<(i32, usize, Vec<u32>)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| fuzzy_score(pattern, f).map(|(s, p)| (s, i, p)))
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(files[a.1].len().cmp(&files[b.1].len())));
    hits.truncate(limit);
    hits.into_iter()
        .map(|(score, i, positions)| FileMatch {
            path: files[i].clone(),
            score,
            positions,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_ranks_and_bounds() {
        let files: Vec<String> = [
            "README.md",
            "src/main.rs",
            "src/domain/main_window.rs",
            "docs/maintenance.md",
            "tests/m/a/i/n.rs",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let hits = search(&files, "main", 10);
        assert_eq!(hits[0].path, "src/main.rs");
        assert_eq!(hits.len(), 4);
        assert_eq!(search(&files, "main", 2).len(), 2);
        assert_eq!(search(&files, "", 3).len(), 3);
        assert!(search(&files, "zzz", 3).is_empty());
    }
}
