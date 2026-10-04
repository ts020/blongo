//! Which GitHub repository a project pushes to, read from its git remote.

use std::path::Path;
use std::process::Stdio;

/// `owner/name` on a host, as read from a remote URL. Owner and name are
/// limited to the characters GitHub allows (letters, digits, `-`, `_`,
/// `.`), so they can be put into API paths and GraphQL strings as they
/// are.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepoRef {
    /// Lowercased host name (`github.com`, a GitHub Enterprise host).
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    /// Parse `https://host/owner/name(.git)`, `ssh://git@host[:port]/owner/name`,
    /// `git@host:owner/name.git` and `git://host/owner/name`. `None`: not a
    /// URL of that shape (a local path, a deeper path, odd characters).
    pub fn parse(url: &str) -> Option<Self> {
        let url = url.trim();
        let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh") {
                return None;
            }
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            // A port (`host:2222`); IPv6 literals are not GitHub hosts.
            let host = host.split(':').next()?;
            (host, path)
        } else {
            // scp-like: `user@host:owner/name`. A Windows path (`C:\…`)
            // or a relative path has no `@` before the colon.
            let (authority, path) = url.split_once(':')?;
            let (_, host) = authority.split_once('@')?;
            (host, path)
        };
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
        let ok = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let host_ok = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'));
        if !ok(owner) || !ok(name) || !host_ok {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// `owner/name`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// REST API base for the host, without a trailing slash.
    pub fn api_base(&self) -> String {
        if self.host == "github.com" {
            "https://api.github.com".into()
        } else if self.host.ends_with(".ghe.com") {
            format!("https://api.{}", self.host)
        } else {
            format!("https://{}/api/v3", self.host)
        }
    }

    /// GraphQL endpoint for the host.
    pub fn graphql_url(&self) -> String {
        if self.host == "github.com" || self.host.ends_with(".ghe.com") {
            format!("{}/graphql", self.api_base())
        } else {
            format!("https://{}/api/graphql", self.host)
        }
    }

    /// Web URL of a pull request.
    pub fn pull_url(&self, number: u64) -> String {
        format!("https://{}/{}/pull/{number}", self.host, self.full_name())
    }
}

/// The host a REST API base belongs to (`https://api.github.com` →
/// `github.com`, `https://ghe.example/api/v3` → `ghe.example`).
pub fn api_host(api: &str) -> Option<String> {
    let rest = api
        .strip_prefix("https://")
        .or_else(|| api.strip_prefix("http://"))?;
    let host = rest.split(['/', ':']).next()?.to_ascii_lowercase();
    Some(match host.as_str() {
        "api.github.com" => "github.com".to_owned(),
        h if h.starts_with("api.") && h.ends_with(".ghe.com") => h["api.".len()..].to_owned(),
        _ => host,
    })
}

/// The URL of the remote a folder pushes to: `origin`, else the first
/// remote. Read from the configuration as written (`url.*.insteadOf`
/// rewrites are not applied), so the forge repository is the one the user
/// named. `None`: not a git folder, or no remote.
pub async fn remote_url(cwd: &Path) -> Option<String> {
    if let Some(url) = git(cwd, &["config", "--get", "remote.origin.url"]).await {
        return Some(url);
    }
    let remotes = git(cwd, &["remote"]).await?;
    let first = remotes.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    git(cwd, &["config", "--get", &format!("remote.{first}.url")]).await
}

/// Name of the remote [`remote_url`] reads (`origin` when it exists).
pub async fn remote_name(cwd: &Path) -> Option<String> {
    if git(cwd, &["config", "--get", "remote.origin.url"])
        .await
        .is_some()
    {
        return Some("origin".into());
    }
    let remotes = git(cwd, &["remote"]).await?;
    remotes
        .lines()
        .next()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
}

/// The branch checked out in `cwd` (`None`: detached, not a repository).
pub async fn current_branch(cwd: &Path) -> Option<String> {
    git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await
}

/// `branch` has been pushed: it has an upstream, or the remote [`remote_url`]
/// reads has a remote-tracking branch of that name. Local only (no
/// network), so threads that never pushed cost no API call.
pub async fn branch_pushed(cwd: &Path, branch: &str) -> bool {
    if branch.starts_with('-') {
        return false;
    }
    if git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{branch}@{{upstream}}"),
        ],
    )
    .await
    .is_some()
    {
        return true;
    }
    let Some(remote) = remote_name(cwd).await else {
        return false;
    };
    git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote}/{branch}"),
        ],
    )
    .await
    .is_some()
}

/// Commits on the checked-out `branch` that its remote-tracking branch
/// lacks, and the other way round, as of the last fetch (no network).
/// `None`: `branch` is not checked out in `cwd` or has no remote branch.
pub async fn ahead_behind(cwd: &Path, branch: &str) -> Option<(u32, u32)> {
    if branch.starts_with('-') || current_branch(cwd).await.as_deref() != Some(branch) {
        return None;
    }
    // The branch's upstream (a fork remote too), else origin's branch.
    let upstream = format!("{branch}@{{upstream}}");
    let target = if git(cwd, &["rev-parse", "--verify", "--quiet", &upstream])
        .await
        .is_some()
    {
        upstream
    } else {
        format!("refs/remotes/{}/{branch}", remote_name(cwd).await?)
    };
    let counts = git(
        cwd,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{target}"),
        ],
    )
    .await?;
    let (ahead, behind) = counts.split_once(char::is_whitespace)?;
    Some((ahead.trim().parse().ok()?, behind.trim().parse().ok()?))
}

/// Files changed in `cwd` and not committed (untracked ones included,
/// ignored ones not).
pub async fn uncommitted(cwd: &Path) -> u32 {
    git(cwd, &["status", "--porcelain", "-z"])
        .await
        .map_or(0, |out| {
            // Renames carry their old path as an extra field.
            let mut n = 0;
            let mut fields = out.split('\0').filter(|f| !f.is_empty());
            while let Some(f) = fields.next() {
                n += 1;
                if f.starts_with('R') || f.starts_with('C') {
                    fields.next();
                }
            }
            n
        })
}

async fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(host: &str, owner: &str, name: &str) -> Option<RepoRef> {
        Some(RepoRef {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        })
    }

    #[test]
    fn parses_the_usual_remote_shapes() {
        let want = r("github.com", "ts020", "blongo");
        assert_eq!(RepoRef::parse("https://github.com/ts020/blongo.git"), want);
        assert_eq!(RepoRef::parse("https://github.com/ts020/blongo"), want);
        assert_eq!(RepoRef::parse("https://github.com/ts020/blongo/"), want);
        assert_eq!(RepoRef::parse("git@github.com:ts020/blongo.git"), want);
        assert_eq!(
            RepoRef::parse("ssh://git@github.com:22/ts020/blongo.git"),
            want
        );
        assert_eq!(
            RepoRef::parse("https://user:tok@GitHub.com/ts020/blongo"),
            want
        );
        assert_eq!(
            RepoRef::parse("git@ghe.example.com:team/app.js.git"),
            r("ghe.example.com", "team", "app.js")
        );
    }

    #[test]
    fn refuses_paths_and_odd_names() {
        for bad in [
            "/srv/git/blongo.git",
            "../blongo",
            "C:\\repos\\blongo",
            "file:///srv/git/a/b",
            "https://github.com/ts020",
            "https://github.com/a/b/c",
            "https://github.com/a\"b/c",
            "https://github.com/../c",
            "git@github.com:a/b c",
            "",
        ] {
            assert_eq!(RepoRef::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn endpoints_per_host() {
        let gh = RepoRef::parse("https://github.com/a/b").unwrap();
        assert_eq!(gh.api_base(), "https://api.github.com");
        assert_eq!(gh.graphql_url(), "https://api.github.com/graphql");
        assert_eq!(gh.pull_url(7), "https://github.com/a/b/pull/7");
        let ghe = RepoRef::parse("https://ghe.corp/a/b").unwrap();
        assert_eq!(ghe.api_base(), "https://ghe.corp/api/v3");
        assert_eq!(ghe.graphql_url(), "https://ghe.corp/api/graphql");
        let cloud = RepoRef::parse("https://acme.ghe.com/a/b").unwrap();
        assert_eq!(cloud.api_base(), "https://api.acme.ghe.com");
        assert_eq!(cloud.graphql_url(), "https://api.acme.ghe.com/graphql");
        assert_eq!(
            api_host("https://api.github.com").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            api_host("https://ghe.corp/api/v3").as_deref(),
            Some("ghe.corp")
        );
        assert_eq!(
            api_host("https://api.acme.ghe.com").as_deref(),
            Some("acme.ghe.com")
        );
    }
}
