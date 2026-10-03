//! Domain types shared by the core, the harnesses and the UI.
//!
//! Phase 0 only carries what the harness spikes emit. The full command/event
//! model (Project → Thread → Run → TurnItem) lands in Phase 1.

use serde::{Deserialize, Serialize};

/// Which agent CLI a harness drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Codex,
    ClaudeCode,
    Antigravity,
}

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
