//! Agent harnesses: spawn an agent CLI and translate its wire protocol into
//! [`blongo_protocol::AgentEvent`]s. Initial scope is Codex, Claude Code and
//! Antigravity only.
//!
//! Phase 0 spike API (one shape for all three agents):
//!
//! - `start(config)` spawns the CLI in `config.cwd` and returns a [`Session`].
//! - [`Session::prompt`] sends a user turn (queued while a turn runs).
//! - [`Session::events`] is a bounded `tokio::sync::mpsc` stream of
//!   [`AgentEvent`]s; a slow consumer back-pressures the agent's stdout
//!   instead of growing a buffer.
//! - [`Session::approve`] answers an [`AgentEvent::ApprovalRequest`].
//! - [`Session::interrupt`] cancels the running turn; the turn still ends
//!   with `TurnCompleted { status: Interrupted }`.
//! - [`Session::shutdown`] (or dropping the session) reaps the child.
//!
//! Memory rules: one driver task + one stdin writer task + one stderr drain
//! task per session (tokio tasks, never OS threads), deltas are forwarded as
//! they arrive and nothing keeps the transcript.

use std::ffi::OsString;
use std::path::PathBuf;

use blongo_protocol::AgentEvent;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub mod acp;
pub mod antigravity_install;
pub mod claude;
pub mod codex;
pub(crate) mod jsonrpc;
pub mod process;
pub mod provider;

pub use provider::{StartOptions, start};

/// Capacity of the per-session event channel. Small on purpose: deltas are
/// tiny and a full channel just pauses reading the child's stdout.
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// How to launch one agent session.
#[derive(Clone, Debug, Default)]
pub struct SessionConfig {
    /// Working directory of the agent (the project / worktree).
    pub cwd: PathBuf,
    /// Agent executable. `None` resolves the harness default (env override,
    /// then `PATH`).
    pub executable: Option<PathBuf>,
    /// Extra CLI arguments appended after the harness's own.
    pub extra_args: Vec<OsString>,
    /// Extra environment for the child.
    pub env: Vec<(OsString, OsString)>,
    /// Environment variables removed from the child (e.g. ambient API keys).
    pub env_remove: Vec<OsString>,
    /// Model id, passed through as the agent spells it.
    pub model: Option<String>,
}

impl SessionConfig {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            ..Self::default()
        }
    }

    pub fn executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// The user's answer to an [`AgentEvent::ApprovalRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Allow this and similar requests for the rest of the session, where
    /// the agent supports it (otherwise treated as [`Self::Allow`]).
    AllowForSession,
    Deny,
}

/// Host → driver commands.
#[derive(Debug)]
pub(crate) enum Command {
    Prompt(String),
    /// Add input to the running turn (see `SteerMode`); a prompt when idle.
    Steer(String),
    Approve {
        request_id: String,
        decision: ApprovalDecision,
    },
    Interrupt,
    /// Drop the provider's turn `before_turn` and everything after it
    /// (only for providers with `live_rollback`; others ignore it).
    Rewind {
        before_turn: String,
    },
}

/// A live agent session. Dropping it closes the command channel, which makes
/// the driver close stdin and reap the child.
pub struct Session {
    /// Normalized events, in wire order.
    pub events: mpsc::Receiver<AgentEvent>,
    commands: mpsc::UnboundedSender<Command>,
    pid: Option<u32>,
    driver: JoinHandle<()>,
}

impl Session {
    pub(crate) fn new(
        events: mpsc::Receiver<AgentEvent>,
        commands: mpsc::UnboundedSender<Command>,
        pid: Option<u32>,
        driver: JoinHandle<()>,
    ) -> Self {
        Self {
            events,
            commands,
            pid,
            driver,
        }
    }

    /// Send a user prompt. While a turn runs it is queued and sent as the
    /// next turn (Claude folds it into the running turn itself).
    pub fn prompt(&self, text: impl Into<String>) -> anyhow::Result<()> {
        self.send(Command::Prompt(text.into()))
    }

    /// Add input to the running turn (the provider's steering). When no turn
    /// runs it is an ordinary prompt. The turn still ends with one
    /// `TurnCompleted`.
    pub fn steer(&self, text: impl Into<String>) -> anyhow::Result<()> {
        self.send(Command::Steer(text.into()))
    }

    /// Roll the provider conversation back to before `before_turn` (a
    /// provider turn id). Only meaningful with `live_rollback`.
    pub fn rewind(&self, before_turn: impl Into<String>) -> anyhow::Result<()> {
        self.send(Command::Rewind {
            before_turn: before_turn.into(),
        })
    }

    /// Answer an approval request by the id from `ApprovalRequest`.
    pub fn approve(
        &self,
        request_id: impl Into<String>,
        decision: ApprovalDecision,
    ) -> anyhow::Result<()> {
        self.send(Command::Approve {
            request_id: request_id.into(),
            decision,
        })
    }

    /// Cancel the running turn (no-op when idle).
    pub fn interrupt(&self) -> anyhow::Result<()> {
        self.send(Command::Interrupt)
    }

    /// PID of the agent child (for RSS measurement).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Next event, `None` once the agent exited and the driver finished.
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// Close the session and wait until the child is reaped.
    pub async fn shutdown(self) {
        let Session {
            events,
            commands,
            driver,
            ..
        } = self;
        drop(commands);
        drop(events);
        let _ = driver.await;
    }

    fn send(&self, command: Command) -> anyhow::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("agent session has ended"))
    }
}

/// Compact one-line JSON for approval details / tool payload previews,
/// bounded so a huge tool input never becomes a huge UI string.
pub(crate) fn preview_json(value: &serde_json::Value, limit: usize) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => truncate(s, limit),
        other => truncate(&other.to_string(), limit),
    }
}

/// Keep at most `limit` bytes (on a char boundary), marking the cut.
pub(crate) fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("界界界", 4), "界…");
        assert_eq!(preview_json(&serde_json::json!({"a": 1}), 100), "{\"a\":1}");
        assert_eq!(preview_json(&serde_json::Value::Null, 100), "");
    }
}
