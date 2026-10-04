//! Talking to code forges (GitHub, GitLab) from both ends of Blongo.
//!
//! - [`http`]: the system curl in a subprocess (no HTTP/TLS stack in the
//!   binary; tokens travel on curl's stdin).
//! - [`forge`]: the review inbox and the token file (`forge.json`).
//! - [`remote`], [`auth`], [`github`]: what the core needs to follow a
//!   thread's pull request: which repository a project pushes to, a token
//!   for its host (`forge.json`, else the GitHub CLI's), and the GitHub
//!   API calls themselves.
//! - [`fs`]: owner-only files and folders.

// The PR detail test builds a deeply nested JSON literal.
#![cfg_attr(test, recursion_limit = "256")]

use std::path::PathBuf;

pub mod auth;
pub mod forge;
pub mod fs;
pub mod github;
pub mod http;
pub mod remote;

/// Blongo's configuration folder: `BLONGO_CONFIG_DIR`, else the platform
/// config dir + `blongo`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("BLONGO_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|d| d.join("blongo")))
        .unwrap_or_else(|| PathBuf::from(".blongo-config"))
}
