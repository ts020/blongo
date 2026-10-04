//! Which GitHub token Blongo uses for a host: the one saved in
//! `forge.json` when its API base belongs to that host, else the GitHub
//! CLI's (`gh auth token --hostname H`). The token stays in this process;
//! it reaches curl through its stdin and never crosses the remote wire.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::forge::{ForgeKind, TokenFile};
use crate::remote::api_host;

const GH_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a token came from, for messages ("signed in with gh").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenSource {
    ForgeFile,
    GhCli,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub token: String,
    pub source: TokenSource,
}

/// The token for `host` (lowercase, as in [`crate::remote::RepoRef`]).
/// `None`: neither source has one; the caller shows the PR as unreachable
/// with a hint to sign in. `gh` names the GitHub CLI program (`None`:
/// `gh` on the `PATH`).
pub async fn token_for(host: &str, forge_file: &Path, gh: Option<&Path>) -> Option<Token> {
    if let Some(token) = from_file(host, forge_file) {
        return Some(Token {
            token,
            source: TokenSource::ForgeFile,
        });
    }
    from_gh(host, gh).await.map(|token| Token {
        token,
        source: TokenSource::GhCli,
    })
}

fn from_file(host: &str, path: &Path) -> Option<String> {
    let file = TokenFile::load(path).ok()?;
    let forge = file.get(ForgeKind::GitHub)?;
    (api_host(&forge.api).as_deref() == Some(host)).then(|| forge.token.clone())
}

/// `gh auth token --hostname H`. A missing gh, a signed-out host or a
/// hang (10 s) all read as "no token".
async fn from_gh(host: &str, program: Option<&Path>) -> Option<String> {
    let program = program.unwrap_or(Path::new("gh"));
    let child = tokio::process::Command::new(program)
        .args(["auth", "token", "--hostname", host])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let out = tokio::time::timeout(GH_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    // A token is one printable word; anything else is not one.
    (!token.is_empty() && token.len() < 1024 && token.chars().all(|c| c.is_ascii_graphic()))
        .then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_file_token_only_for_its_host() {
        let dir = std::env::temp_dir().join(format!("blongo-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forge.json");
        let mut file = TokenFile::default();
        file.set(ForgeKind::GitHub, None, "tok-dotcom".into());
        file.save(&path).unwrap();
        assert_eq!(
            from_file("github.com", &path).as_deref(),
            Some("tok-dotcom")
        );
        assert_eq!(from_file("ghe.corp", &path), None);
        file.set(
            ForgeKind::GitHub,
            Some("https://ghe.corp/api/v3".into()),
            "tok-ghe".into(),
        );
        file.save(&path).unwrap();
        assert_eq!(from_file("ghe.corp", &path).as_deref(), Some("tok-ghe"));
        assert_eq!(from_file("github.com", &path), None);
        assert_eq!(from_file("github.com", &dir.join("missing.json")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
