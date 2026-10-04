//! PR / MR review inbox for GitHub and GitLab with the user's own token.
//!
//! Requests go through [`crate::http`] (the system curl). Tokens live in
//! `forge.json` in the config directory, owner-only (0600), never in the
//! settings file, the environment or a command line. The API base is
//! configurable (GitHub Enterprise, self-hosted GitLab, and the tests'
//! fake servers on loopback).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::http::{self, Request};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
        }
    }

    pub fn default_api(self) -> &'static str {
        match self {
            Self::GitHub => "https://api.github.com",
            Self::GitLab => "https://gitlab.com/api/v4",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forge {
    pub kind: ForgeKind,
    /// API base without a trailing slash.
    pub api: String,
    pub token: String,
}

/// `forge.json`: one entry per forge kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenFile {
    #[serde(default)]
    pub forges: Vec<Forge>,
}

pub fn default_path() -> PathBuf {
    crate::environments::config_dir().join("forge.json")
}

impl TokenFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::secret::write_private(path, &bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn get(&self, kind: ForgeKind) -> Option<&Forge> {
        self.forges
            .iter()
            .find(|f| f.kind == kind && !f.token.is_empty())
    }

    /// Set (or with an empty token, remove) a forge's token.
    pub fn set(&mut self, kind: ForgeKind, api: Option<String>, token: String) {
        self.forges.retain(|f| f.kind != kind);
        if !token.trim().is_empty() {
            self.forges.push(Forge {
                kind,
                api: api
                    .filter(|a| !a.trim().is_empty())
                    .unwrap_or_else(|| kind.default_api().to_owned())
                    .trim_end_matches('/')
                    .to_owned(),
                token: token.trim().to_owned(),
            });
        }
    }
}

/// One pull / merge request waiting for the user's review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub kind: ForgeKind,
    /// `owner/repo` (GitHub) or the project path (GitLab).
    pub repo: String,
    /// GitLab project id (GitHub: unused).
    pub project_id: Option<u64>,
    pub number: u64,
    pub title: String,
    pub author: String,
    pub url: String,
    pub updated_at: String,
}

impl PullRequest {
    pub fn key(&self) -> String {
        match self.kind {
            ForgeKind::GitHub => format!("{}#{}", self.repo, self.number),
            ForgeKind::GitLab => format!("{}!{}", self.repo, self.number),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFile {
    pub path: String,
    pub old_path: Option<String>,
    /// `A`, `M`, `D`, `R`.
    pub status: char,
    pub added: u32,
    pub removed: u32,
    /// Unified diff hunks (`@@ … @@` and lines); `None` for binary or
    /// too-large files.
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrDetail {
    pub files: Vec<PrFile>,
    /// Commit the comments are anchored to.
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
}

/// A comment on a line of the new version of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

/// Most PRs / files fetched.
const PAGE: u32 = 50;
const MAX_FILES: u32 = 300;

impl Forge {
    fn request(&self, req: Request) -> Request {
        match self.kind {
            ForgeKind::GitHub => req
                .header("Authorization", format!("Bearer {}", self.token))
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
            ForgeKind::GitLab => req.header("PRIVATE-TOKEN", self.token.clone()),
        }
    }

    async fn get(&self, path: &str) -> Result<Value, String> {
        let resp = http::send(self.request(Request::get(format!("{}{path}", self.api)))).await?;
        if !resp.ok() {
            return Err(api_error(self.kind, resp.status, &resp.text()));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|e| format!("{}: bad JSON: {e}", self.kind.label()))
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let resp =
            http::send(self.request(Request::post_json(format!("{}{path}", self.api), body)))
                .await?;
        if !resp.ok() {
            return Err(api_error(self.kind, resp.status, &resp.text()));
        }
        Ok(serde_json::from_slice(&resp.body).unwrap_or(Value::Null))
    }

    /// Open PRs / MRs where the user's review is requested.
    pub async fn inbox(&self) -> Result<Vec<PullRequest>, String> {
        match self.kind {
            ForgeKind::GitHub => {
                let v = self
                    .get(&format!(
                        "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3A%40me&per_page={PAGE}"
                    ))
                    .await?;
                Ok(v["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|i| {
                        let repo = i["repository_url"]
                            .as_str()?
                            .rsplitn(3, '/')
                            .take(2)
                            .collect::<Vec<_>>();
                        Some(PullRequest {
                            kind: ForgeKind::GitHub,
                            repo: format!("{}/{}", repo.get(1)?, repo.first()?),
                            project_id: None,
                            number: i["number"].as_u64()?,
                            title: i["title"].as_str().unwrap_or("").to_owned(),
                            author: i["user"]["login"].as_str().unwrap_or("").to_owned(),
                            url: i["html_url"].as_str().unwrap_or("").to_owned(),
                            updated_at: i["updated_at"].as_str().unwrap_or("").to_owned(),
                        })
                    })
                    .collect())
            }
            ForgeKind::GitLab => {
                let me = self.get("/user").await?;
                let id = me["id"].as_u64().ok_or("GitLab: no user id")?;
                let v = self
                    .get(&format!(
                        "/merge_requests?state=opened&scope=all&reviewer_id={id}&per_page={PAGE}"
                    ))
                    .await?;
                Ok(v.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| {
                        Some(PullRequest {
                            kind: ForgeKind::GitLab,
                            repo: m["references"]["full"]
                                .as_str()
                                .and_then(|r| r.split('!').next())
                                .unwrap_or("")
                                .to_owned(),
                            project_id: m["project_id"].as_u64(),
                            number: m["iid"].as_u64()?,
                            title: m["title"].as_str().unwrap_or("").to_owned(),
                            author: m["author"]["username"].as_str().unwrap_or("").to_owned(),
                            url: m["web_url"].as_str().unwrap_or("").to_owned(),
                            updated_at: m["updated_at"].as_str().unwrap_or("").to_owned(),
                        })
                    })
                    .collect())
            }
        }
    }

    /// Changed files with their patches and the commits to anchor
    /// comments to.
    pub async fn detail(&self, pr: &PullRequest) -> Result<PrDetail, String> {
        match self.kind {
            ForgeKind::GitHub => {
                let base = format!("/repos/{}/pulls/{}", pr.repo, pr.number);
                let head = self.get(&base).await?;
                let files = self
                    .get(&format!("{base}/files?per_page={MAX_FILES}"))
                    .await?;
                Ok(PrDetail {
                    head_sha: head["head"]["sha"].as_str().unwrap_or("").to_owned(),
                    base_sha: head["base"]["sha"].as_str().map(str::to_owned),
                    start_sha: None,
                    files: files
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|f| PrFile {
                            path: f["filename"].as_str().unwrap_or("").to_owned(),
                            old_path: f["previous_filename"].as_str().map(str::to_owned),
                            status: match f["status"].as_str() {
                                Some("added") => 'A',
                                Some("removed") => 'D',
                                Some("renamed") => 'R',
                                _ => 'M',
                            },
                            added: f["additions"].as_u64().unwrap_or(0) as u32,
                            removed: f["deletions"].as_u64().unwrap_or(0) as u32,
                            patch: f["patch"].as_str().map(str::to_owned),
                        })
                        .collect(),
                })
            }
            ForgeKind::GitLab => {
                let project = pr.project_id.ok_or("GitLab: no project id")?;
                let v = self
                    .get(&format!(
                        "/projects/{project}/merge_requests/{}/changes",
                        pr.number
                    ))
                    .await?;
                let refs = &v["diff_refs"];
                Ok(PrDetail {
                    head_sha: refs["head_sha"].as_str().unwrap_or("").to_owned(),
                    base_sha: refs["base_sha"].as_str().map(str::to_owned),
                    start_sha: refs["start_sha"].as_str().map(str::to_owned),
                    files: v["changes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| {
                            let patch = c["diff"].as_str().filter(|d| !d.is_empty());
                            let (added, removed) = patch.map(count_lines).unwrap_or((0, 0));
                            PrFile {
                                path: c["new_path"].as_str().unwrap_or("").to_owned(),
                                old_path: c["old_path"]
                                    .as_str()
                                    .filter(|o| Some(*o) != c["new_path"].as_str())
                                    .map(str::to_owned),
                                status: if c["new_file"].as_bool() == Some(true) {
                                    'A'
                                } else if c["deleted_file"].as_bool() == Some(true) {
                                    'D'
                                } else if c["renamed_file"].as_bool() == Some(true) {
                                    'R'
                                } else {
                                    'M'
                                },
                                added,
                                removed,
                                patch: patch.map(str::to_owned),
                            }
                        })
                        .collect(),
                })
            }
        }
    }

    /// Post `comments` (and an optional summary) as one review.
    pub async fn review(
        &self,
        pr: &PullRequest,
        detail: &PrDetail,
        summary: &str,
        comments: &[ReviewComment],
    ) -> Result<(), String> {
        match self.kind {
            ForgeKind::GitHub => {
                let body = json!({
                    "commit_id": detail.head_sha,
                    "event": "COMMENT",
                    "body": summary,
                    "comments": comments.iter().map(|c| json!({
                        "path": c.path, "line": c.line, "side": "RIGHT", "body": c.body,
                    })).collect::<Vec<_>>(),
                });
                self.post(
                    &format!("/repos/{}/pulls/{}/reviews", pr.repo, pr.number),
                    &body,
                )
                .await
                .map(|_| ())
            }
            ForgeKind::GitLab => {
                let project = pr.project_id.ok_or("GitLab: no project id")?;
                let base = format!("/projects/{project}/merge_requests/{}", pr.number);
                for c in comments {
                    self.post(
                        &format!("{base}/discussions"),
                        &json!({
                            "body": c.body,
                            "position": {
                                "position_type": "text",
                                "base_sha": detail.base_sha,
                                "start_sha": detail.start_sha,
                                "head_sha": detail.head_sha,
                                "new_path": c.path,
                                "new_line": c.line,
                            }
                        }),
                    )
                    .await?;
                }
                if !summary.trim().is_empty() {
                    self.post(&format!("{base}/notes"), &json!({ "body": summary }))
                        .await?;
                }
                Ok(())
            }
        }
    }
}

fn count_lines(patch: &str) -> (u32, u32) {
    let mut added = 0;
    let mut removed = 0;
    for line in patch.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

fn api_error(kind: ForgeKind, status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    match status {
        401 => format!("{}: the token was refused (401): {message}", kind.label()),
        403 => format!("{}: not allowed (403): {message}", kind.label()),
        _ => format!("{}: HTTP {status}: {message}", kind.label()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_sets_and_removes() {
        let mut file = TokenFile::default();
        file.set(ForgeKind::GitHub, None, " ghp_x ".into());
        file.set(
            ForgeKind::GitLab,
            Some("https://git.example/api/v4/".into()),
            "glpat".into(),
        );
        assert_eq!(file.get(ForgeKind::GitHub).unwrap().token, "ghp_x");
        assert_eq!(
            file.get(ForgeKind::GitLab).unwrap().api,
            "https://git.example/api/v4"
        );
        file.set(ForgeKind::GitHub, None, String::new());
        assert!(file.get(ForgeKind::GitHub).is_none());
        assert_eq!(count_lines("@@ -1 +1,2 @@\n-a\n+b\n+c\n"), (2, 1));
    }
}
