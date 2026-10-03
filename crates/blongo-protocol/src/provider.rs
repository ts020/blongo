//! Providers (agent CLIs) and what each can do.
//!
//! Behaviour above the harness branches on [`ProviderCapabilities`], never
//! on the provider's name (a t3code invariant): adding a provider means
//! describing it here, not sprinkling `match provider` through the core.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Which agent CLI drives a thread.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Codex,
    ClaudeCode,
    Antigravity,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [Self::Codex, Self::ClaudeCode, Self::Antigravity];

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::ClaudeCode => "Claude Code",
            Self::Antigravity => "Antigravity",
        }
    }

    /// Stable id (database, CLI flags).
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Antigravity => "antigravity",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }

    pub fn capabilities(self) -> ProviderCapabilities {
        match self {
            // codex app-server: `turn/steer`, `thread/resume`, `thread/fork`
            // (lastTurnId), `thread/revert` (beforeTurnId), model per turn,
            // `turn/plan/updated`.
            Self::Codex => ProviderCapabilities {
                version: CAPABILITIES_VERSION,
                steer: SteerMode::Native,
                resume: true,
                native_fork: true,
                native_rollback: true,
                live_rollback: true,
                model_switch: ModelSwitch::PerTurn,
                approval_for_session: true,
                plans: true,
                interactive_login: false,
            },
            // claude CLI: a `priority: "now"` user message steers;
            // `--resume`, `--fork-session`, `--resume-session-at`; `--model`
            // is fixed for a process; TodoWrite is the plan.
            Self::ClaudeCode => ProviderCapabilities {
                version: CAPABILITIES_VERSION,
                steer: SteerMode::Native,
                resume: true,
                native_fork: true,
                native_rollback: true,
                live_rollback: false,
                model_switch: ModelSwitch::RestartSession,
                approval_for_session: true,
                plans: true,
                interactive_login: false,
            },
            // ACP: no in-turn input, so steering cancels the prompt and
            // resends (t3code's recorded behaviour); `session/load` resumes
            // when the agent advertises it; no fork / rewind in ACP v1.
            Self::Antigravity => ProviderCapabilities {
                version: CAPABILITIES_VERSION,
                steer: SteerMode::CancelAndResend,
                resume: true,
                native_fork: false,
                native_rollback: false,
                live_rollback: false,
                model_switch: ModelSwitch::RestartSession,
                approval_for_session: true,
                plans: true,
                // OAuth through `authenticate` + a browser URL.
                interactive_login: true,
            },
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Bumped whenever a field's meaning changes.
pub const CAPABILITIES_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub version: u32,
    /// How a message sent while a turn runs reaches the agent.
    pub steer: SteerMode,
    /// The provider can continue its own conversation in a new process.
    pub resume: bool,
    /// The provider can branch its conversation at a turn (fork keeps the
    /// provider's full context instead of a text handoff).
    pub native_fork: bool,
    /// The provider can drop its turns after a given one (rollback keeps
    /// the provider in sync with the app's run count).
    pub native_rollback: bool,
    /// The rollback applies to a running session (otherwise the session is
    /// restarted with the rewind as its start context).
    pub live_rollback: bool,
    pub model_switch: ModelSwitch,
    pub approval_for_session: bool,
    /// Blongo can run the provider's sign-in itself (otherwise the user
    /// signs in with the CLI).
    pub interactive_login: bool,
    /// Reports a plan / todo list.
    pub plans: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerMode {
    /// Input is added to the running turn.
    Native,
    /// The running prompt is cancelled and the new input sent right after,
    /// within the same app run.
    CancelAndResend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSwitch {
    /// The model is a per-turn parameter.
    PerTurn,
    /// The model is fixed for a process: switching restarts (and resumes)
    /// the session.
    RestartSession,
}

/// One model a provider offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// What the provider expects in its model parameter.
    pub id: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    /// Normalize the providers' spellings.
    pub fn parse(s: &str) -> Self {
        match s {
            "completed" | "complete" | "done" => Self::Completed,
            "in_progress" | "inProgress" | "running" | "active" => Self::InProgress,
            _ => Self::Pending,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_capabilities_differ() {
        for p in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(p.id()), Some(p));
            assert_eq!(p.capabilities().version, CAPABILITIES_VERSION);
        }
        assert_eq!(
            serde_json::to_string(&ProviderKind::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
        assert!(!ProviderKind::Antigravity.capabilities().native_fork);
        assert_eq!(
            ProviderKind::Antigravity.capabilities().steer,
            SteerMode::CancelAndResend
        );
        assert_eq!(PlanStatus::parse("inProgress"), PlanStatus::InProgress);
        assert_eq!(PlanStatus::parse("whatever"), PlanStatus::Pending);
    }
}
