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
//! - continuing: `--resume <session>`; native fork adds `--fork-session`;
//!   rewind / fork-at-turn add `--resume-session-at=<assistant uuid>` (the
//!   flags the Agent SDK passes for `resume`, `forkSession` and
//!   `resumeSessionAt`). The provider turn id is the uuid of the turn's last
//!   top-level `assistant` message, which is what `--resume-session-at`
//!   takes.
//! - steering: a user message with `"priority": "now"` while a turn runs.
//!   The CLI ends the running turn early (`result` error_during_execution)
//!   and answers the steer in a follow-up turn; both are one app turn here.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::time::Duration;

use blongo_protocol::{AgentEvent, ModelInfo, PlanStatus, PlanStep, TurnStatus};
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
    /// `--resume`: continue this session.
    pub resume: Option<String>,
    /// `--fork-session`: continue `resume` as a new session.
    pub fork_session: bool,
    /// `--resume-session-at`: drop everything after this assistant message.
    pub resume_session_at: Option<String>,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            permission_mode: "default".into(),
            kill_grace: Duration::from_secs(3),
            resume: None,
            fork_session: false,
            resume_session_at: None,
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
        // One argument, so a value starting with `-` is never read as a flag.
        args.push(format!("--model={model}"));
    }
    if let Some(mcp) = &config.mcp {
        // `=`: the flag takes a list, a separate value could swallow more.
        let servers = serde_json::json!({ "mcpServers": { mcp.name.clone(): mcp.json() } });
        args.push(format!("--mcp-config={servers}"));
    }
    if let Some(session) = &options.resume {
        args.push("--resume".into());
        args.push(session.clone());
        if options.fork_session {
            args.push("--fork-session".into());
        }
        if let Some(at) = &options.resume_session_at {
            args.push(format!("--resume-session-at={at}"));
        }
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

/// Token usage and cost of a `result` frame (input counts the cached and
/// cache-creation tokens too).
fn result_usage(frame: &Value) -> Option<blongo_protocol::Usage> {
    let usage = frame.get("usage")?;
    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = n("cache_read_input_tokens");
    Some(blongo_protocol::Usage {
        input_tokens: n("input_tokens") + n("cache_creation_input_tokens") + cached,
        output_tokens: n("output_tokens"),
        cached_input_tokens: cached,
        cost_micros: frame
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .map(|usd| (usd * 1e6).round() as u64),
    })
}

/// Request id of the startup `initialize` control request.
const INIT_REQUEST_ID: &str = "blongo-init";

/// `{"type":"user",...}` stdin line.
pub fn user_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// A steering message: delivered into the running turn.
pub fn steer_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
        "priority": "now",
    })
    .to_string()
}

/// The SDK's handshake; its answer lists commands and models.
fn initialize_line() -> String {
    json!({
        "type": "control_request",
        "request_id": INIT_REQUEST_ID,
        "request": { "subtype": "initialize", "hooks": null },
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
    tool_use_id: Option<String>,
}

/// The `can_use_tool` reply for a decision.
fn permission_response(pending: PendingApproval, decision: ApprovalDecision) -> Value {
    let mut response = match decision {
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
    };
    if let Some(id) = pending.tool_use_id {
        response["toolUseID"] = json!(id);
    }
    response
}

/// Stateful stdout-frame → event normalizer (no I/O, unit-testable).
#[derive(Default)]
pub(crate) struct Normalizer {
    session_id: Option<String>,
    approvals: HashMap<String, PendingApproval>,
    /// User prompts written but not yet answered by a `result`.
    turns_in_flight: usize,
    interrupted: bool,
    /// Message id of the partial message being streamed.
    streaming_message: Option<String>,
    /// Messages whose text / thinking arrived as deltas; their `assistant`
    /// frames must not repeat it. Cleared at each `result`.
    streamed_text: HashSet<String>,
    streamed_thinking: HashSet<String>,
    /// uuid of the last top-level `assistant` frame of the turn.
    last_assistant_uuid: Option<String>,
    /// TodoWrite calls (their results are not tool rows).
    todo_calls: HashSet<String>,
}

impl Normalizer {
    pub(crate) fn frame(&mut self, line: &str, out: &mut Vec<AgentEvent>) {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        // Subagent (Task tool) frames carry a parent id; only the top-level
        // conversation is shown.
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
                let event = &frame["event"];
                if event.get("type").and_then(Value::as_str) == Some("message_start") {
                    self.streaming_message = event
                        .pointer("/message/id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                let delta = &event["delta"];
                // Without a `message_start` the id is "", which also
                // matches an `assistant` frame that carries no id.
                let streamed = |set: &mut HashSet<String>, current: &Option<String>| {
                    set.insert(current.clone().unwrap_or_default());
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            streamed(&mut self.streamed_text, &self.streaming_message);
                            out.push(AgentEvent::TextDelta { text: text.into() });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            streamed(&mut self.streamed_thinking, &self.streaming_message);
                            out.push(AgentEvent::ReasoningDelta { text: text.into() });
                        }
                    }
                    _ => {}
                }
            }
            // Text normally streamed as deltas already; without partial
            // messages it only arrives here.
            "assistant" if !is_subagent => {
                if let Some(uuid) = frame.get("uuid").and_then(Value::as_str) {
                    self.last_assistant_uuid = Some(uuid.to_owned());
                }
                let message_id = frame
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                for block in blocks(&frame) {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") if !self.streamed_text.contains(message_id) => {
                            if let Some(text) = block
                                .get("text")
                                .and_then(Value::as_str)
                                .filter(|t| !t.is_empty())
                            {
                                out.push(AgentEvent::TextDelta { text: text.into() });
                            }
                        }
                        Some("thinking") if !self.streamed_thinking.contains(message_id) => {
                            if let Some(text) = block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .filter(|t| !t.is_empty())
                            {
                                out.push(AgentEvent::ReasoningDelta { text: text.into() });
                            }
                        }
                        Some("tool_use") => self.tool_use(block, out),
                        _ => {}
                    }
                }
            }
            "user" if !is_subagent => {
                for block in blocks(&frame) {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let call_id = str_of(block, "tool_use_id");
                        if self.todo_calls.remove(&call_id) {
                            continue;
                        }
                        out.push(AgentEvent::ToolResult {
                            call_id,
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
            "control_response" => self.control_response(&frame, out),
            "control_cancel_request" => {
                if let Some(id) = frame.get("request_id").and_then(Value::as_str) {
                    self.approvals.remove(id);
                }
            }
            _ => {}
        }
    }

    fn tool_use(&mut self, block: &Value, out: &mut Vec<AgentEvent>) {
        let call_id = str_of(block, "id");
        let name = str_of(block, "name");
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        // TodoWrite is the agent's plan: show it as one, not as a tool row.
        if name == "TodoWrite" {
            let steps = input
                .get("todos")
                .and_then(Value::as_array)
                .map(|todos| {
                    todos
                        .iter()
                        .filter_map(|t| {
                            Some(PlanStep {
                                text: t.get("content").and_then(Value::as_str)?.to_owned(),
                                status: PlanStatus::parse(
                                    t.get("status").and_then(Value::as_str).unwrap_or(""),
                                ),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.todo_calls.insert(call_id);
            out.push(AgentEvent::Plan { steps });
            return;
        }
        out.push(AgentEvent::ToolCall {
            call_id,
            name,
            input,
        });
    }

    fn result(&mut self, frame: &Value, out: &mut Vec<AgentEvent>) {
        self.turns_in_flight = self.turns_in_flight.saturating_sub(1);
        self.streamed_text.clear();
        self.streamed_thinking.clear();
        self.streaming_message = None;
        // Approvals of a finished turn can no longer be answered.
        self.approvals.clear();
        if self.turns_in_flight > 0 {
            // A steer is still being answered: the CLI cut the first part
            // of the turn short. It is one app turn, so nothing ends yet.
            return;
        }
        let subtype = frame.get("subtype").and_then(Value::as_str).unwrap_or("");
        let is_error = frame
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(uuid) = self.last_assistant_uuid.take() {
            out.push(AgentEvent::ProviderTurnId { id: uuid });
        }
        if let Some(usage) = result_usage(frame) {
            out.push(AgentEvent::Usage(usage));
        }
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
                tool_use_id: request
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        );
    }

    /// Answers to our own control requests: only `initialize` matters (it
    /// lists the models).
    fn control_response(&mut self, frame: &Value, out: &mut Vec<AgentEvent>) {
        let response = &frame["response"];
        if response.get("request_id").and_then(Value::as_str) != Some(INIT_REQUEST_ID) {
            return;
        }
        let models: Vec<ModelInfo> = response
            .pointer("/response/models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| {
                        let id = m.get("value").and_then(Value::as_str)?;
                        let label = m.get("displayName").and_then(Value::as_str).unwrap_or(id);
                        Some(ModelInfo {
                            id: id.to_owned(),
                            label: label.to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !models.is_empty() {
            out.push(AgentEvent::Models { models });
        }
    }
}

fn is_auth_failure(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("/login")
        || m.contains("invalid api key")
        || m.contains("not logged in")
        || m.contains("failed to authenticate")
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
    proc.write_line(initialize_line());
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
                        // Exiting after an interrupt request honours it.
                        let status = if norm.interrupted {
                            TurnStatus::Interrupted
                        } else {
                            let message =
                                process::crash_message("claude", &mut proc.child, &proc.stderr);
                            let _ = events.send(AgentEvent::Error { message }).await;
                            TurnStatus::Failed
                        };
                        let _ = events.send(AgentEvent::TurnCompleted { status }).await;
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
                Some(Command::Steer { id, text }) => {
                    if norm.turns_in_flight > 0 && !norm.interrupted {
                        // If the CLI finishes the turn before reading it, it
                        // answers it as a further turn; `turns_in_flight`
                        // folds that into the same app turn.
                        norm.turns_in_flight += 1;
                        proc.write_line(steer_message_line(&text));
                    } else {
                        // Never a turn of its own: the host decides.
                        let _ = events.send(AgentEvent::SteerNotDelivered { id }).await;
                    }
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
                // Rewinds need a restart with `--resume-session-at`.
                Some(Command::Rewind { .. }) => {}
                None => break 'main,
            },
        }
    }
    let _ = proc.stdin.send(StdinMsg::Close);
    process::terminate(&mut proc.child, options.kill_grace).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_server_goes_into_one_mcp_config_argument() {
        let mut config = SessionConfig::new("/tmp");
        config.mcp = Some(crate::McpServer {
            name: "blongo".into(),
            command: "/usr/bin/blongo".into(),
            args: vec!["mcp-bridge".into(), "/s".into(), "/t.token".into()],
        });
        let args = args(&config, &ClaudeOptions::default());
        let flag = args
            .iter()
            .find_map(|a| a.strip_prefix("--mcp-config="))
            .expect("--mcp-config");
        let v: Value = serde_json::from_str(flag).unwrap();
        assert_eq!(v["mcpServers"]["blongo"]["command"], "/usr/bin/blongo");
        assert_eq!(v["mcpServers"]["blongo"]["args"][2], "/t.token");
    }

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
            model: Some("fake-default".into()),
            ..SessionConfig::default()
        };
        let args = args(&config, &ClaudeOptions::default());
        let joined = args.join(" ");
        assert!(joined.starts_with("-p --input-format stream-json --output-format stream-json"));
        assert!(joined.contains("--permission-prompt-tool stdio --permission-mode default"));
        assert!(joined.ends_with("--model=fake-default"));
    }

    #[test]
    fn resume_fork_and_rewind_flags() {
        let options = ClaudeOptions {
            resume: Some("s1".into()),
            fork_session: true,
            resume_session_at: Some("u9".into()),
            ..ClaudeOptions::default()
        };
        let joined = args(&SessionConfig::default(), &options).join(" ");
        assert!(joined.ends_with("--resume s1 --fork-session --resume-session-at=u9"));
        let fresh = args(&SessionConfig::default(), &ClaudeOptions::default()).join(" ");
        assert!(!fresh.contains("--resume"));
    }

    #[test]
    fn unstreamed_assistant_text_plan_and_turn_id() {
        let (_, events) = run(&[
            r#"{"type":"assistant","uuid":"u1","message":{"id":"m1","content":[{"type":"thinking","thinking":""},{"type":"text","text":"from frame"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m2"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"streamed"}}}"#,
            r#"{"type":"assistant","uuid":"u2","message":{"id":"m2","content":[{"type":"text","text":"streamed"}]}}"#,
            r#"{"type":"assistant","uuid":"u3","message":{"id":"m3","content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"Read","status":"completed"},{"content":"Write","status":"in_progress"}]}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"blongo-init","response":{"models":[{"value":"default","displayName":"Default"},{"value":"m-x"}]}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ]);
        assert_eq!(
            events,
            vec![
                AgentEvent::TextDelta {
                    text: "from frame".into()
                },
                AgentEvent::TextDelta {
                    text: "streamed".into()
                },
                AgentEvent::Plan {
                    steps: vec![
                        PlanStep {
                            text: "Read".into(),
                            status: PlanStatus::Completed
                        },
                        PlanStep {
                            text: "Write".into(),
                            status: PlanStatus::InProgress
                        },
                    ]
                },
                AgentEvent::Models {
                    models: vec![
                        ModelInfo {
                            id: "default".into(),
                            label: "Default".into()
                        },
                        ModelInfo {
                            id: "m-x".into(),
                            label: "m-x".into()
                        },
                    ]
                },
                AgentEvent::ProviderTurnId { id: "u3".into() },
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed
                },
            ]
        );
    }

    #[test]
    fn steered_turn_ends_once() {
        let mut norm = Normalizer {
            turns_in_flight: 2,
            ..Normalizer::default()
        };
        let mut out = Vec::new();
        norm.frame(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#,
            &mut out,
        );
        assert!(out.is_empty(), "{out:?}");
        norm.frame(
            r#"{"type":"assistant","uuid":"u5","message":{"id":"m","content":[{"type":"text","text":"steered"}]}}"#,
            &mut out,
        );
        norm.frame(r#"{"type":"result","subtype":"success"}"#, &mut out);
        assert_eq!(
            out,
            vec![
                AgentEvent::TextDelta {
                    text: "steered".into()
                },
                AgentEvent::ProviderTurnId { id: "u5".into() },
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed
                },
            ]
        );
        let steer: Value = serde_json::from_str(&steer_message_line("x")).unwrap();
        assert_eq!(steer["priority"], "now");
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
            tool_use_id: Some("toolu_1".into()),
        };
        assert_eq!(
            permission_response(pending(), ApprovalDecision::Allow),
            json!({"behavior": "allow", "updatedInput": {"command": "ls"}, "toolUseID": "toolu_1"})
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
