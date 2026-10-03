//! Claude Code harness: drives the installed `claude` CLI directly over its
//! stream-json stdio protocol (no Agent SDK, no Node sidecar).
//!
//! Wire (adapted from zeron crates/harness/src/claude/{mod,wire}.rs):
//! - launch: `claude -p --input-format stream-json --output-format stream-json
//!   --verbose --include-partial-messages --permission-prompt-tool stdio
//!   --permission-mode <mode>`
//! - stdin: one `{"type":"user","message":{...}}` line per prompt; control
//!   responses and the `interrupt` control request.
//! - stdout: `system/init` (session id), `stream_event` content deltas,
//!   `assistant` (tool_use blocks), `user` (tool_result blocks), `result`
//!   (turn end) and `control_request` `can_use_tool` (approval).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::time::Duration;

use blongo_protocol::{AgentEvent, TurnStatus};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::process::{self, AgentProcess, StdinMsg};
use crate::{ApprovalDecision, Command, Session, SessionConfig, preview_json};

/// Env var naming the `claude` executable (tests point it at a fake CLI).
pub const EXECUTABLE_ENV: &str = "BLONGO_CLAUDE_EXECUTABLE";

/// Tool output is capped before it becomes an event.
const MAX_TOOL_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ClaudeOptions {
    /// `--permission-mode`. `default` (hidden alias accepted by 2.1.288)
    /// asks through the control channel; `bypassPermissions` never asks.
    pub permission_mode: String,
    /// Grace between SIGTERM and SIGKILL on shutdown.
    pub kill_grace: Duration,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            permission_mode: "default".into(),
            kill_grace: Duration::from_secs(3),
        }
    }
}

/// CLI arguments for a session (exposed for tests).
pub fn args(config: &SessionConfig, options: &ClaudeOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        // Mandatory with -p + stream-json output.
        "--verbose",
        "--include-partial-messages",
        // Route permission prompts to the stdio control channel
        // (`can_use_tool`); undocumented but what the Agent SDK uses.
        "--permission-prompt-tool",
        "stdio",
        "--permission-mode",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(options.permission_mode.clone());
    if let Some(model) = &config.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    args
}

pub async fn start(config: SessionConfig) -> anyhow::Result<Session> {
    start_with(config, ClaudeOptions::default()).await
}

pub async fn start_with(config: SessionConfig, options: ClaudeOptions) -> anyhow::Result<Session> {
    let exe = process::resolve_executable(config.executable.as_deref(), EXECUTABLE_ENV, "claude")?;
    let args = args(&config, &options);
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let proc = process::spawn(&exe, &args, &config, &[])?;
    let pid = proc.pid();
    let (event_tx, event_rx) = mpsc::channel(crate::EVENT_CHANNEL_CAPACITY);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let driver = tokio::spawn(drive(proc, cmd_rx, event_tx, options));
    Ok(Session::new(event_rx, cmd_tx, pid, driver))
}

/// `{"type":"user",...}` stdin line.
pub fn user_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
    })
    .to_string()
}

fn control_response_line(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
    .to_string()
}

fn interrupt_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    })
    .to_string()
}

/// A `can_use_tool` request awaiting the user.
struct PendingApproval {
    input: Value,
    suggestions: Option<Value>,
}

/// The `can_use_tool` reply for a decision.
fn permission_response(pending: PendingApproval, decision: ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Deny => json!({
            "behavior": "deny",
            "message": "The user denied this tool call.",
        }),
        ApprovalDecision::Allow => json!({ "behavior": "allow", "updatedInput": pending.input }),
        ApprovalDecision::AllowForSession => {
            let mut allow = json!({ "behavior": "allow", "updatedInput": pending.input });
            if let Some(suggestions) = pending.suggestions {
                allow["updatedPermissions"] = suggestions;
            }
            allow
        }
    }
}

/// Stateful stdout-frame → event normalizer (no I/O, unit-testable).
#[derive(Default)]
pub(crate) struct Normalizer {
    session_id: Option<String>,
    approvals: HashMap<String, PendingApproval>,
    /// User prompts written but not yet answered by a `result`.
    turns_in_flight: usize,
    interrupted: bool,
}

impl Normalizer {
    pub(crate) fn frame(&mut self, line: &str, out: &mut Vec<AgentEvent>) {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        // Subagent (Task tool) frames carry a parent id; the spike keeps
        // only the top-level conversation.
        let is_subagent = frame
            .get("parent_tool_use_id")
            .is_some_and(|p| !p.is_null());
        match kind {
            "system" => {
                if frame.get("subtype").and_then(Value::as_str) == Some("init")
                    && let Some(id) = frame.get("session_id").and_then(Value::as_str)
                    && self.session_id.as_deref() != Some(id)
                {
                    self.session_id = Some(id.to_owned());
                    out.push(AgentEvent::SessionStarted {
                        provider_session_id: id.to_owned(),
                    });
                }
            }
            "stream_event" if !is_subagent => {
                let delta = &frame["event"]["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            out.push(AgentEvent::TextDelta { text: text.into() });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            out.push(AgentEvent::ReasoningDelta { text: text.into() });
                        }
                    }
                    _ => {}
                }
            }
            // Assistant text already streamed as deltas; only tool_use here.
            "assistant" if !is_subagent => {
                for block in blocks(&frame) {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        out.push(AgentEvent::ToolCall {
                            call_id: str_of(block, "id"),
                            name: str_of(block, "name"),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        });
                    }
                }
            }
            "user" if !is_subagent => {
                for block in blocks(&frame) {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        out.push(AgentEvent::ToolResult {
                            call_id: str_of(block, "tool_use_id"),
                            is_error: block
                                .get("is_error")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            output: tool_result_text(block.get("content")),
                            exit_code: None,
                        });
                    }
                }
            }
            "result" => self.result(&frame, out),
            "control_request" => self.control_request(&frame, out),
            "control_cancel_request" => {
                if let Some(id) = frame.get("request_id").and_then(Value::as_str) {
                    self.approvals.remove(id);
                }
            }
            _ => {}
        }
    }

    fn result(&mut self, frame: &Value, out: &mut Vec<AgentEvent>) {
        self.turns_in_flight = self.turns_in_flight.saturating_sub(1);
        let subtype = frame.get("subtype").and_then(Value::as_str).unwrap_or("");
        let is_error = frame
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status = if std::mem::take(&mut self.interrupted) {
            TurnStatus::Interrupted
        } else if subtype == "success" && !is_error {
            TurnStatus::Completed
        } else {
            let message = frame
                .get("result")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    frame
                        .get("errors")
                        .and_then(Value::as_array)
                        .and_then(|e| e.first())
                        .map(|e| preview_json(e, 2000))
                })
                .unwrap_or_else(|| format!("Claude turn failed ({subtype})"));
            if is_auth_failure(&message) {
                out.push(AgentEvent::AuthRequired { message, url: None });
            } else {
                out.push(AgentEvent::Error { message });
            }
            TurnStatus::Failed
        };
        // Approvals of a finished turn can no longer be answered.
        self.approvals.clear();
        out.push(AgentEvent::TurnCompleted { status });
    }

    fn control_request(&mut self, frame: &Value, out: &mut Vec<AgentEvent>) {
        let request_id = str_of(frame, "request_id");
        let request = &frame["request"];
        if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
            return;
        }
        let tool = str_of(request, "tool_name");
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let detail = match input.get("command").and_then(Value::as_str) {
            Some(command) => crate::truncate(command, 4000),
            None => preview_json(&input, 4000),
        };
        out.push(AgentEvent::ApprovalRequest {
            request_id: request_id.clone(),
            title: format!("Allow {tool}?"),
            detail,
        });
        self.approvals.insert(
            request_id,
            PendingApproval {
                input,
                suggestions: request.get("permission_suggestions").cloned(),
            },
        );
    }
}

fn is_auth_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("/login") || m.contains("invalid api key") || m.contains("not logged in")
}

fn blocks(frame: &Value) -> impl Iterator<Item = &Value> {
    frame["message"]["content"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn tool_result_text(content: Option<&Value>) -> String {
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    crate::truncate(&text, MAX_TOOL_OUTPUT)
}

async fn drive(
    mut proc: AgentProcess,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<AgentEvent>,
    options: ClaudeOptions,
) {
    let mut norm = Normalizer::default();
    let mut batch = Vec::new();
    let mut interrupt_seq = 0u64;
    'main: loop {
        tokio::select! {
            line = proc.stdout.next_line() => match line {
                Ok(Some(line)) => {
                    norm.frame(&line, &mut batch);
                    for event in batch.drain(..) {
                        if events.send(event).await.is_err() {
                            break 'main;
                        }
                    }
                }
                Ok(None) => {
                    if norm.turns_in_flight > 0 {
                        let message = process::crash_message("claude", &mut proc.child, &proc.stderr);
                        let _ = events.send(AgentEvent::Error { message }).await;
                        let _ = events
                            .send(AgentEvent::TurnCompleted { status: TurnStatus::Failed })
                            .await;
                    }
                    break 'main;
                }
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    let _ = events.send(AgentEvent::Error { message: e.to_string() }).await;
                }
                Err(_) => break 'main,
            },
            command = commands.recv() => match command {
                Some(Command::Prompt(text)) => {
                    norm.turns_in_flight += 1;
                    proc.write_line(user_message_line(&text));
                }
                Some(Command::Approve { request_id, decision }) => {
                    if let Some(pending) = norm.approvals.remove(&request_id) {
                        let response = permission_response(pending, decision);
                        proc.write_line(control_response_line(&request_id, response));
                    }
                }
                Some(Command::Interrupt) => {
                    if norm.turns_in_flight > 0 && !norm.interrupted {
                        norm.interrupted = true;
                        interrupt_seq += 1;
                        proc.write_line(interrupt_line(&format!("blongo-interrupt-{interrupt_seq}")));
                    }
                }
                None => break 'main,
            },
        }
    }
    let _ = proc.stdin.send(StdinMsg::Close);
    process::terminate(&mut proc.child, options.kill_grace).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> (Normalizer, Vec<AgentEvent>) {
        let mut norm = Normalizer {
            turns_in_flight: 1,
            ..Normalizer::default()
        };
        let mut out = Vec::new();
        for line in lines {
            norm.frame(line, &mut out);
        }
        (norm, out)
    }

    #[test]
    fn args_route_permissions_to_stdio() {
        let config = SessionConfig {
            model: Some("haiku".into()),
            ..SessionConfig::default()
        };
        let args = args(&config, &ClaudeOptions::default());
        let joined = args.join(" ");
        assert!(joined.starts_with("-p --input-format stream-json --output-format stream-json"));
        assert!(joined.contains("--permission-prompt-tool stdio --permission-mode default"));
        assert!(joined.ends_with("--model haiku"));
    }

    #[test]
    fn normalizes_a_tool_turn() {
        let (norm, events) = run(&[
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":"sub","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"sub"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a\nb"}]}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s1"}"#,
        ]);
        assert_eq!(
            events,
            vec![
                AgentEvent::SessionStarted {
                    provider_session_id: "s1".into()
                },
                AgentEvent::ReasoningDelta { text: "hmm".into() },
                AgentEvent::TextDelta { text: "Hi".into() },
                AgentEvent::ToolCall {
                    call_id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls"})
                },
                AgentEvent::ApprovalRequest {
                    request_id: "r1".into(),
                    title: "Allow Bash?".into(),
                    detail: "ls".into()
                },
                AgentEvent::ToolResult {
                    call_id: "t1".into(),
                    is_error: false,
                    output: "a\nb".into(),
                    exit_code: None,
                },
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed
                },
            ]
        );
        assert_eq!(norm.turns_in_flight, 0);
        assert!(norm.approvals.is_empty());
    }

    #[test]
    fn errors_and_interrupts() {
        let (_, events) = run(&[
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}"#,
        ]);
        assert!(matches!(
            &events[0],
            AgentEvent::AuthRequired { url: None, .. }
        ));
        assert_eq!(
            events[1],
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed
            }
        );

        let mut norm = Normalizer {
            interrupted: true,
            ..Normalizer::default()
        };
        let mut out = Vec::new();
        norm.frame(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#,
            &mut out,
        );
        assert_eq!(
            out,
            vec![AgentEvent::TurnCompleted {
                status: TurnStatus::Interrupted
            }]
        );
    }

    #[test]
    fn permission_responses() {
        let pending = || PendingApproval {
            input: json!({"command": "ls"}),
            suggestions: Some(json!([{"type": "addRules"}])),
        };
        assert_eq!(
            permission_response(pending(), ApprovalDecision::Allow),
            json!({"behavior": "allow", "updatedInput": {"command": "ls"}})
        );
        assert_eq!(
            permission_response(pending(), ApprovalDecision::AllowForSession)["updatedPermissions"],
            json!([{"type": "addRules"}])
        );
        assert_eq!(
            permission_response(pending(), ApprovalDecision::Deny)["behavior"],
            "deny"
        );
        let line: Value = serde_json::from_str(&control_response_line("r1", json!({}))).unwrap();
        assert_eq!(line["response"]["request_id"], "r1");
        assert_eq!(line["response"]["subtype"], "success");
    }
}
