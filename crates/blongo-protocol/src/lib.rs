//! Domain types shared by the core, the harnesses and the UI.
//!
//! - [`domain`]: the orchestration model (Project → Thread → Run → TurnItem),
//!   commands and sequenced domain events.
//! - [`AgentEvent`]: what a harness emits, normalized at the provider edge.

use serde::{Deserialize, Serialize};

pub mod domain;
pub use domain::*;

pub mod provider;
pub use provider::*;

pub mod client;
pub mod wire;
pub mod workspace;

/// One normalized event from an agent turn. Harnesses translate their
/// provider's wire format into these at the edge; nothing above the harness
/// looks at provider-shaped JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The provider assigned (or resumed) its own session/thread id.
    SessionStarted {
        provider_session_id: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        is_error: bool,
        output: String,
        /// Process exit code, for command executions that report one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// The agent is blocked until the user answers.
    ApprovalRequest {
        request_id: String,
        title: String,
        detail: String,
    },
    /// The agent CLI is not signed in. `url` is a browser sign-in link when
    /// the agent offered one (Antigravity prints an OAuth URL on stdout).
    AuthRequired {
        message: String,
        url: Option<String>,
    },
    /// The provider's own id for the current turn (Codex turn id, Claude
    /// assistant message uuid at the end of the turn). Native fork and
    /// rollback refer to turns by it.
    ProviderTurnId {
        id: String,
    },
    /// A steer (see `Session::steer`) did not reach a running turn: no turn
    /// was running, the turn ended (or was interrupted) first, or the
    /// provider refused it. The harness never turns a steer into a turn of
    /// its own; the host decides what to do with the text.
    SteerNotDelivered {
        id: String,
    },
    /// The agent's current plan / todo list for this turn (replaces the
    /// previous one).
    Plan {
        steps: Vec<PlanStep>,
    },
    /// Models the provider offers (from its own handshake, never a list
    /// compiled into Blongo).
    Models {
        models: Vec<ModelInfo>,
    },
    /// Token usage of the current turn so far (replaces earlier reports
    /// of the same turn).
    Usage(Usage),
    TurnCompleted {
        status: TurnStatus,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}
