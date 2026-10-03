//! Agent Client Protocol (ACP) harness over stdio, with the Antigravity
//! (`agy_acp_server`) quirks as data on [`AcpAgent`].
//!
//! Protocol (ACP v1, hand-rolled on serde_json; see docs/phase0/
//! spike-antigravity.md for why not the `agent-client-protocol` crate):
//! `initialize` → (`authenticate`) → `session/new` → per prompt
//! `session/prompt`, whose *response* (`stopReason`) ends the turn while
//! `session/update` notifications stream chunks and tool calls;
//! `session/request_permission` is the approval request; interrupt is the
//! `session/cancel` notification.
//!
//! Antigravity quirks (adapted from zeron crates/harness/src/acp/mod.rs and
//! t3code apps/server/src/provider/acp/{AntigravityAcpSupport,
//! AntigravityProtocol}.ts and provider/antigravityAuthSupport.ts):
//! - Linux builds run with `--uid=` (as the ACP registry launches them).
//! - When not signed in, `authenticate` makes the server print
//!   `Open the following link to authenticate the ACP server: <url>` on
//!   *stdout* (not JSON) and try to open a browser. The stdout transform
//!   turns that line into [`AgentEvent::AuthRequired`]; `BROWSER=true`
//!   keeps Python's `webbrowser` from opening anything.
//! - Tool payloads can carry multi-MB outputs and base64 images; session
//!   updates are bounded/sanitized before they become events.
//! - Native questions reuse `session/request_permission` with a
//!   `toolCallId` starting `interaction_`.
//!
//! Phase 2: `session/load` resumes a session when the agent advertises
//! `loadSession` (replayed history is dropped); steering has no native
//! method, so it is `session/cancel` followed by a new `session/prompt`
//! (the ACP registry's `message_steering` recording), folded into a single
//! turn; `plan` updates become [`AgentEvent::Plan`]; `session/new` models
//! become [`AgentEvent::Models`] and `session/set_model` selects one.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use blongo_protocol::{AgentEvent, ModelInfo, PlanStatus, PlanStep, TurnStatus};
use serde_json::{Map, Value, json};
use tokio::process::ChildStdout;
use tokio::sync::mpsc;

use crate::jsonrpc::{AUTH_REQUIRED_CODE, CallError, Incoming, RpcOut, RpcPeer};
use crate::process::{self, AgentProcess};
use crate::{ApprovalDecision, Command, Session, SessionConfig, preview_json};

/// Bound for one tool text field (t3code's `TOOL_TEXT_LIMIT`).
const TOOL_TEXT_LIMIT: usize = 8_000;

/// Static description of one ACP agent.
#[derive(Clone, Debug)]
pub struct AcpAgent {
    pub display_name: &'static str,
    /// Env var overriding the executable.
    pub executable_env: &'static str,
    /// Executable name looked up on PATH.
    pub executable_name: &'static str,
    /// Extra candidate locations checked before PATH (managed installs).
    pub extra_paths: Vec<PathBuf>,
    pub args: Vec<&'static str>,
    pub env: Vec<(&'static str, &'static str)>,
    pub env_remove: Vec<&'static str>,
    /// `authenticate` method to call eagerly when the agent advertises it.
    pub auth_method: Option<&'static str>,
    /// Prefix of a non-JSON stdout line that carries a sign-in URL.
    pub auth_url_prefix: Option<&'static str>,
    /// Apply Antigravity's session-update bounding.
    pub sanitize_updates: bool,
    /// Timeout for initialize/authenticate/session/new (cold starts of the
    /// 1.9 GB Antigravity `.par` are slow).
    pub setup_timeout: Duration,
    pub kill_grace: Duration,
    /// Session to continue with `session/load` (when advertised).
    pub resume_session: Option<String>,
}

pub const ANTIGRAVITY_AUTH_PREFIX: &str =
    "Open the following link to authenticate the ACP server: ";
pub const ANTIGRAVITY_EXECUTABLE_ENV: &str = "BLONGO_ANTIGRAVITY_EXECUTABLE";
/// Registry manifest listing Google's current builds.
pub const ANTIGRAVITY_REGISTRY_URL: &str = "https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json";

/// The archive's entry point (a Python `.par` bundle on Unix).
pub const ANTIGRAVITY_ENTRY: &str = if cfg!(windows) {
    "agy_acp_server.exe"
} else {
    "agy_acp_server.par"
};

/// Antigravity's ACP server (`agy_acp_server`).
pub fn antigravity() -> AcpAgent {
    AcpAgent {
        display_name: "Antigravity",
        executable_env: ANTIGRAVITY_EXECUTABLE_ENV,
        executable_name: ANTIGRAVITY_ENTRY,
        extra_paths: antigravity_managed_entry().into_iter().collect(),
        args: if cfg!(target_os = "linux") {
            vec!["--uid="]
        } else {
            Vec::new()
        },
        env: vec![
            ("PYTHONUNBUFFERED", "1"),
            // Keep OAuth tokens in GEMINI_HOME files, not the OS keyring.
            ("AGY_ACP_FORCE_FILE_STORAGE", "1"),
            // `true` "opens" the URL successfully without a browser; we
            // surface the URL from stdout instead.
            ("BROWSER", "true"),
        ],
        // Ambient Google keys would switch the agent to API billing.
        env_remove: vec![
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_LOCATION",
            "GOOGLE_GENAI_USE_VERTEXAI",
            "AGY_ACP_ENABLE_OAUTH",
        ],
        auth_method: Some("oauth-personal"),
        auth_url_prefix: Some(ANTIGRAVITY_AUTH_PREFIX),
        sanitize_updates: true,
        setup_timeout: Duration::from_secs(120),
        kill_grace: Duration::from_secs(3),
        resume_session: None,
    }
}

/// Where Blongo would unpack the downloaded archive:
/// `$XDG_DATA_HOME/blongo/antigravity-acp/current/agy_acp_server.par`.
pub fn antigravity_managed_entry() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(
        base.join("blongo/antigravity-acp/current")
            .join(ANTIGRAVITY_ENTRY),
    )
}

/// ACP registry platform key for this build.
pub fn registry_platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("macos", "aarch64") => "darwin-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        _ => return None,
    })
}

/// One downloadable build from the registry manifest.
#[derive(Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub archive_url: String,
    pub cmd: String,
    pub args: Vec<String>,
}

/// Pick this platform's archive from the registry `agent.json`.
pub fn release_from_manifest(manifest: &Value, platform: &str) -> Option<Release> {
    let entry = manifest.pointer(&format!("/distribution/binary/{platform}"))?;
    let archive_url = entry.get("archive")?.as_str()?;
    // Only Google-hosted archives are acceptable.
    if !archive_url.starts_with("https://dl.google.com/") {
        return None;
    }
    Some(Release {
        version: manifest.get("version")?.as_str()?.to_owned(),
        archive_url: archive_url.to_owned(),
        cmd: entry
            .get("cmd")?
            .as_str()?
            .trim_start_matches("./")
            .to_owned(),
        args: entry
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn resolve(config: &SessionConfig, agent: &AcpAgent) -> std::io::Result<PathBuf> {
    if config.executable.is_none()
        && std::env::var_os(agent.executable_env).is_none()
        && let Some(found) = agent.extra_paths.iter().find(|p| p.is_file())
    {
        return Ok(found.clone());
    }
    process::resolve_executable(
        config.executable.as_deref(),
        agent.executable_env,
        agent.executable_name,
    )
}

pub async fn start(config: SessionConfig, agent: AcpAgent) -> anyhow::Result<Session> {
    let exe = resolve(&config, &agent)?;
    let args: Vec<&OsStr> = agent.args.iter().map(OsStr::new).collect();
    let mut config = config;
    config
        .env_remove
        .extend(agent.env_remove.iter().map(Into::into));
    let proc = process::spawn(&exe, &args, &config, &agent.env)?;
    let pid = proc.pid();
    let (event_tx, event_rx) = mpsc::channel(crate::EVENT_CHANNEL_CAPACITY);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let driver = tokio::spawn(drive(proc, cmd_rx, event_tx, config, agent));
    Ok(Session::new(event_rx, cmd_tx, pid, driver))
}

pub(crate) fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientInfo": { "name": "blongo", "title": "Blongo", "version": env!("CARGO_PKG_VERSION") },
        // Declined: the agent uses its own fs and terminal.
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        },
    })
}

/// The URL in an Antigravity sign-in stdout line.
pub fn auth_url(prefix: &str, line: &str) -> Option<String> {
    let url = line.strip_prefix(prefix)?.trim();
    (url.starts_with("https://") && url.len() <= 16_384).then(|| url.to_owned())
}

struct PendingPermission {
    id: Value,
    options: Vec<Value>,
    question: bool,
}

/// Session state machine (no I/O except through `RpcOut`).
pub(crate) struct Driver {
    session_id: String,
    sanitize: bool,
    prompt_request: Option<i64>,
    cancel_sent: bool,
    /// Text to send once the cancelled prompt answers (a steer).
    steer: Option<String>,
    queued: VecDeque<String>,
    permissions: HashMap<String, PendingPermission>,
    permission_seq: u64,
}

impl Driver {
    pub(crate) fn new(session_id: String, sanitize: bool) -> Self {
        Self {
            session_id,
            sanitize,
            prompt_request: None,
            cancel_sent: false,
            steer: None,
            queued: VecDeque::new(),
            permissions: HashMap::new(),
            permission_seq: 0,
        }
    }

    pub(crate) fn turn_active(&self) -> bool {
        self.prompt_request.is_some()
    }

    fn send_prompt(&mut self, rpc: &mut RpcOut, text: &str) {
        let id = rpc.request(
            "session/prompt",
            json!({
                "sessionId": self.session_id,
                "prompt": [{ "type": "text", "text": text }],
            }),
        );
        self.prompt_request = Some(id);
        self.cancel_sent = false;
    }

    pub(crate) fn command(&mut self, rpc: &mut RpcOut, command: Command) {
        match command {
            Command::Prompt(text) => {
                if self.turn_active() {
                    self.queued.push_back(text);
                } else {
                    self.send_prompt(rpc, &text);
                }
            }
            Command::Approve {
                request_id,
                decision,
            } => {
                if let Some(pending) = self.permissions.remove(&request_id) {
                    let outcome = match pick_option(&pending, decision) {
                        Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
                        None => json!({ "outcome": "cancelled" }),
                    };
                    rpc.respond(&pending.id, json!({ "outcome": outcome }));
                }
            }
            Command::Steer(text) => {
                if !self.turn_active() {
                    self.send_prompt(rpc, &text);
                } else {
                    // Several steers before the cancel lands are joined.
                    self.steer = Some(match self.steer.take() {
                        Some(prev) => format!("{prev}\n\n{text}"),
                        None => text,
                    });
                    self.cancel(rpc);
                }
            }
            Command::Interrupt => {
                self.steer = None;
                self.cancel(rpc);
            }
            // ACP v1 has no rewind.
            Command::Rewind { .. } => {}
        }
    }

    fn cancel(&mut self, rpc: &mut RpcOut) {
        if self.turn_active() && !self.cancel_sent {
            self.cancel_sent = true;
            // ACP: the client must answer open permission requests with
            // `cancelled` once it cancels the turn.
            for (_, pending) in self.permissions.drain() {
                rpc.respond(
                    &pending.id,
                    json!({ "outcome": { "outcome": "cancelled" } }),
                );
            }
            rpc.notify(
                "session/cancel",
                Some(json!({ "sessionId": self.session_id })),
            );
        }
    }

    pub(crate) fn incoming(&mut self, rpc: &mut RpcOut, msg: Incoming, out: &mut Vec<AgentEvent>) {
        match msg {
            Incoming::Response { id, result } if Some(id) == self.prompt_request => {
                self.prompt_request = None;
                if let Some(text) = self.steer.take() {
                    // The cancelled half of a steer: the turn continues
                    // with the new prompt.
                    self.permissions.clear();
                    self.send_prompt(rpc, &text);
                    return;
                }
                let status = match result {
                    Ok(result) => match result.get("stopReason").and_then(Value::as_str) {
                        Some("cancelled") => TurnStatus::Interrupted,
                        Some("refusal") => {
                            out.push(AgentEvent::Error {
                                message: "The agent refused the request.".into(),
                            });
                            TurnStatus::Failed
                        }
                        _ if self.cancel_sent => TurnStatus::Interrupted,
                        _ => TurnStatus::Completed,
                    },
                    Err(e) => {
                        out.push(AgentEvent::Error {
                            message: format!("session/prompt: {e}"),
                        });
                        if self.cancel_sent {
                            TurnStatus::Interrupted
                        } else {
                            TurnStatus::Failed
                        }
                    }
                };
                self.permissions.clear();
                out.push(AgentEvent::TurnCompleted { status });
                if let Some(next) = self.queued.pop_front() {
                    self.send_prompt(rpc, &next);
                }
            }
            Incoming::Response { .. } => {}
            Incoming::Notification { method, mut params } => {
                if method == "session/update"
                    && params.get("sessionId").and_then(Value::as_str) == Some(&self.session_id)
                {
                    let update = params
                        .get_mut("update")
                        .map(Value::take)
                        .unwrap_or_default();
                    self.update(update, out);
                }
            }
            Incoming::Request { id, method, params } => {
                if method == "session/request_permission" {
                    self.permission(id, params, out);
                } else {
                    rpc.respond_error(&id, -32601, &format!("unsupported by blongo: {method}"));
                }
            }
            Incoming::Text(_) => {}
        }
    }

    fn update(&mut self, mut update: Value, out: &mut Vec<AgentEvent>) {
        if self.sanitize {
            sanitize_update(&mut update);
        }
        map_update(&update, out);
    }

    fn permission(&mut self, id: Value, params: Value, out: &mut Vec<AgentEvent>) {
        let tool = &params["toolCall"];
        let tool_call_id = tool.get("toolCallId").and_then(Value::as_str).unwrap_or("");
        let question = tool_call_id.starts_with("interaction_");
        let options: Vec<Value> = params
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let title = tool
            .get("title")
            .and_then(Value::as_str)
            .map(|t| crate::truncate(t, 500))
            .unwrap_or_else(|| "Allow tool call?".into());
        let detail = if question {
            // Native question: list its choices; Allow picks the first.
            options
                .iter()
                .filter_map(|o| o.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" / ")
        } else {
            tool_command(tool)
                .or_else(|| tool.get("rawInput").map(|r| preview_json(r, 4000)))
                .unwrap_or_default()
        };
        self.permission_seq += 1;
        let request_id = format!("perm-{}", self.permission_seq);
        out.push(AgentEvent::ApprovalRequest {
            request_id: request_id.clone(),
            title,
            detail,
        });
        self.permissions.insert(
            request_id,
            PendingPermission {
                id,
                options,
                question,
            },
        );
    }
}

/// The optionId for a decision (by ACP option `kind`).
fn pick_option(pending: &PendingPermission, decision: ApprovalDecision) -> Option<String> {
    if pending.question {
        return match decision {
            ApprovalDecision::Deny => None,
            _ => pending.options.first().and_then(option_id),
        };
    }
    let by_kind = |kind: &str| {
        pending
            .options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(option_id)
    };
    match decision {
        ApprovalDecision::Allow => by_kind("allow_once").or_else(|| by_kind("allow_always")),
        ApprovalDecision::AllowForSession => {
            by_kind("allow_always").or_else(|| by_kind("allow_once"))
        }
        ApprovalDecision::Deny => by_kind("reject_once").or_else(|| by_kind("reject_always")),
    }
}

fn option_id(option: &Value) -> Option<String> {
    option
        .get("optionId")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// Map one (sanitized) `session/update` to events.
pub(crate) fn map_update(update: &Value, out: &mut Vec<AgentEvent>) {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    let chunk = || {
        update
            .pointer("/content/text")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = chunk() {
                out.push(AgentEvent::TextDelta { text });
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = chunk() {
                out.push(AgentEvent::ReasoningDelta { text });
            }
        }
        "tool_call" => {
            let call_id = str_of(update, "toolCallId");
            let name = update
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .or_else(|| update.get("kind").and_then(Value::as_str))
                .unwrap_or("tool")
                .to_owned();
            let mut input = update.get("rawInput").cloned().unwrap_or(json!({}));
            if let Some(command) = tool_command(update)
                && let Some(obj) = input.as_object_mut()
            {
                obj.entry("command").or_insert(Value::String(command));
            }
            out.push(AgentEvent::ToolCall {
                call_id: call_id.clone(),
                name,
                input,
            });
            tool_result(update, call_id, out);
        }
        "tool_call_update" => tool_result(update, str_of(update, "toolCallId"), out),
        "plan" => {
            let steps = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|e| {
                            Some(PlanStep {
                                text: e.get("content").and_then(Value::as_str)?.to_owned(),
                                status: PlanStatus::parse(
                                    e.get("status").and_then(Value::as_str).unwrap_or(""),
                                ),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(AgentEvent::Plan { steps });
        }
        // available_commands_update, current_mode_update,
        // user_message_chunk, …: not surfaced by the spike.
        _ => {}
    }
}

fn tool_result(update: &Value, call_id: String, out: &mut Vec<AgentEvent>) {
    let status = update.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(status, "completed" | "failed") {
        return;
    }
    let raw = update.get("rawOutput");
    let field = |keys: &[&str]| keys.iter().find_map(|k| raw.and_then(|r| r.get(*k)));
    let exit = field(&["exitCode", "exit_code"]).and_then(Value::as_i64);
    let output = field(&["combinedOutput", "combined_output"])
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let text: Vec<&str> = update
                .get("content")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(|c| c.pointer("/content/text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        })
        .or_else(|| raw.map(|r| preview_json(r, TOOL_TEXT_LIMIT)))
        .unwrap_or_default();
    out.push(AgentEvent::ToolResult {
        call_id,
        is_error: status == "failed" || exit.is_some_and(|c| c != 0),
        output,
        exit_code: exit.and_then(|c| i32::try_from(c).ok()),
    });
}

/// Antigravity spells the shell command several ways.
fn tool_command(tool: &Value) -> Option<String> {
    const KEYS: [&str; 4] = ["CommandLine", "command_line", "commandLine", "command"];
    let from = |v: Option<&Value>| {
        KEYS.iter().find_map(|k| {
            v.and_then(|v| v.get(*k))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| crate::truncate(s, TOOL_TEXT_LIMIT))
        })
    };
    from(tool.get("rawInput")).or_else(|| from(tool.get("rawOutput")))
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Bound a tool `session/update` in place (t3code
/// `normalizeAntigravitySessionUpdate`): long strings keep their tail,
/// base64 images and duplicated formatted output are dropped, and a node
/// and text budget caps the whole payload.
pub fn sanitize_update(update: &mut Value) {
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if !matches!(kind, Some("tool_call" | "tool_call_update")) {
        return;
    }
    let Some(obj) = update.as_object_mut() else {
        return;
    };
    if let Some(Value::String(title)) = obj.get_mut("title") {
        bound_tail(title, TOOL_TEXT_LIMIT);
    }
    for key in ["rawInput", "rawOutput", "_meta"] {
        if let Some(value) = obj.get_mut(key) {
            let mut budget = Budget {
                nodes: 512,
                text: 64_000,
            };
            *value = sanitize_value(value.take(), &mut budget, 0).unwrap_or(Value::Null);
        }
    }
    if let Some(content) = obj.get_mut("content") {
        let mut budget = Budget {
            nodes: 512,
            text: 32_000,
        };
        *content = sanitize_value(content.take(), &mut budget, 0).unwrap_or(json!([]));
    }
}

struct Budget {
    nodes: usize,
    text: usize,
}

const TRUNCATED_MARK: &str = "[Earlier output truncated]\n\n";

/// Keep the last `limit` bytes of `text`.
fn bound_tail(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = format!("{TRUNCATED_MARK}{}", &text[start..]);
}

fn sanitize_value(value: Value, budget: &mut Budget, depth: usize) -> Option<Value> {
    if depth > 12 || budget.nodes == 0 {
        return None;
    }
    budget.nodes -= 1;
    match value {
        Value::String(mut s) => {
            if budget.text == 0 || s.starts_with("data:image/") {
                return None;
            }
            bound_tail(&mut s, TOOL_TEXT_LIMIT.min(budget.text));
            budget.text = budget.text.saturating_sub(s.len());
            Some(Value::String(s))
        }
        Value::Array(items) => {
            let mut kept = Vec::new();
            for item in items {
                if budget.nodes == 0 {
                    break;
                }
                kept.extend(sanitize_value(item, budget, depth + 1));
            }
            Some(Value::Array(kept))
        }
        Value::Object(map) => {
            let is_image = map.get("type").and_then(Value::as_str) == Some("image");
            let image_mime = map
                .get("mimeType")
                .and_then(Value::as_str)
                .is_some_and(|m| m.starts_with("image/"));
            let combined = map
                .get("combinedOutput")
                .or_else(|| map.get("combined_output"))
                .cloned();
            let mut kept = Map::new();
            for (key, entry) in map {
                if budget.nodes == 0 {
                    break;
                }
                let drop = (is_image && (key == "data" || key == "blob"))
                    || (key == "blob" && image_mime)
                    || ((key == "formatted_output" || key == "formattedOutput")
                        && combined.as_ref() == Some(&entry));
                if drop {
                    continue;
                }
                if let Some(entry) = sanitize_value(entry, budget, depth + 1) {
                    kept.insert(key, entry);
                }
            }
            Some(Value::Object(kept))
        }
        other => Some(other),
    }
}

/// initialize → authenticate (if configured and advertised) → session/new.
async fn setup(
    peer: &mut RpcPeer<ChildStdout>,
    config: &SessionConfig,
    agent: &AcpAgent,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<String, String> {
    let timeout = agent.setup_timeout;
    // A sign-in URL on stdout aborts `call` with AUTH_REQUIRED_CODE.
    let describe = |method: &str, e: CallError| match e {
        CallError::Rpc(e) if e.code == AUTH_REQUIRED_CODE => format!("AUTH:{}", e.message),
        other => format!("{method}: {other}"),
    };
    let init = peer
        .call("initialize", initialize_params(), timeout)
        .await
        .map_err(|e| describe("initialize", e))?;
    if let Some(method) = agent.auth_method {
        let advertised = init
            .get("authMethods")
            .and_then(Value::as_array)
            .is_some_and(|m| {
                m.iter()
                    .any(|m| m.get("id").and_then(Value::as_str) == Some(method))
            });
        if advertised
            && let Err(e) = peer
                .call("authenticate", json!({ "methodId": method }), timeout)
                .await
        {
            return Err(describe("authenticate", e));
        }
    }
    let can_load = init
        .pointer("/agentCapabilities/loadSession")
        .and_then(Value::as_bool)
        == Some(true);
    if let Some(resume) = agent.resume_session.as_deref().filter(|_| can_load) {
        let loaded = peer
            .call(
                "session/load",
                json!({
                    "sessionId": resume,
                    "cwd": config.cwd.to_string_lossy(),
                    "mcpServers": [],
                }),
                timeout,
            )
            .await;
        // The agent replays the conversation as updates; Blongo already
        // has it.
        peer.drop_backlog_notifications();
        if let Ok(session) = loaded {
            select_model(peer, resume, &session, config, events, timeout).await;
            return Ok(resume.to_owned());
        }
    }
    let session = peer
        .call(
            "session/new",
            json!({ "cwd": config.cwd.to_string_lossy(), "mcpServers": [] }),
            timeout,
        )
        .await;
    let session = match session {
        Ok(session) => session,
        // ACP's auth_required error code.
        Err(CallError::Rpc(e)) if e.code == -32000 => {
            let _ = events
                .send(AgentEvent::AuthRequired {
                    message: format!("{} needs sign-in: {}", agent.display_name, e.message),
                    url: None,
                })
                .await;
            return Err(String::new());
        }
        Err(e) => return Err(describe("session/new", e)),
    };
    let id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "session/new returned no sessionId".to_owned())?;
    select_model(peer, &id, &session, config, events, timeout).await;
    Ok(id)
}

/// The models a `session/new`/`session/load` result offers.
pub(crate) fn session_models(session: &Value) -> Vec<ModelInfo> {
    session
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|m| {
                    let id = m.get("modelId").and_then(Value::as_str)?;
                    let label = m.get("name").and_then(Value::as_str).unwrap_or(id);
                    Some(ModelInfo {
                        id: id.to_owned(),
                        label: label.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Report the offered models and switch to the configured one.
async fn select_model(
    peer: &mut RpcPeer<ChildStdout>,
    session_id: &str,
    session: &Value,
    config: &SessionConfig,
    events: &mpsc::Sender<AgentEvent>,
    timeout: Duration,
) {
    let models = session_models(session);
    let current = session
        .pointer("/models/currentModelId")
        .and_then(Value::as_str);
    if let Some(model) = &config.model
        && current != Some(model.as_str())
        && models.iter().any(|m| &m.id == model)
        && let Err(e) = peer
            .call(
                "session/set_model",
                json!({ "sessionId": session_id, "modelId": model }),
                timeout,
            )
            .await
    {
        let _ = events
            .send(AgentEvent::Error {
                message: format!("session/set_model: {e}"),
            })
            .await;
    }
    if !models.is_empty() {
        let _ = events.send(AgentEvent::Models { models }).await;
    }
}

/// Interactive sign-in: start the agent, call `authenticate`, report the
/// sign-in URL it prints through `on_url`, and wait (up to `wait`) for the
/// user to finish in the browser. The process is ours and is terminated
/// before returning.
pub async fn login(
    config: SessionConfig,
    agent: AcpAgent,
    on_url: mpsc::Sender<String>,
    wait: Duration,
) -> anyhow::Result<()> {
    let method = agent
        .auth_method
        .ok_or_else(|| anyhow::anyhow!("{} has no sign-in method", agent.display_name))?;
    let exe = resolve(&config, &agent)?;
    let args: Vec<&OsStr> = agent.args.iter().map(OsStr::new).collect();
    let mut config = config;
    config
        .env_remove
        .extend(agent.env_remove.iter().map(Into::into));
    let proc = process::spawn(&exe, &args, &config, &agent.env)?;
    let AgentProcess {
        mut child,
        stdin,
        stdout,
        stderr,
    } = proc;
    let mut peer = RpcPeer::new(RpcOut::new(stdin), stdout);
    let result = async {
        let init = peer
            .call("initialize", initialize_params(), agent.setup_timeout)
            .await
            .map_err(|e| anyhow::anyhow!("initialize: {e}"))?;
        let advertised = init
            .get("authMethods")
            .and_then(Value::as_array)
            .is_some_and(|m| {
                m.iter()
                    .any(|m| m.get("id").and_then(Value::as_str) == Some(method))
            });
        if !advertised {
            anyhow::bail!("{} does not offer {method} sign-in", agent.display_name);
        }
        let id = peer
            .out
            .request("authenticate", json!({ "methodId": method }));
        let wait_for_auth = async {
            loop {
                match peer.next().await {
                    Ok(Some(Incoming::Text(line))) => {
                        if let Some(url) = agent
                            .auth_url_prefix
                            .and_then(|prefix| auth_url(prefix, &line))
                        {
                            let _ = on_url.send(url).await;
                        }
                    }
                    Ok(Some(Incoming::Response { id: got, result })) if got == id => {
                        return result.map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"));
                    }
                    Ok(Some(Incoming::Request { id, method, .. })) => {
                        peer.out.respond_error(
                            &id,
                            -32601,
                            &format!("unsupported by blongo: {method}"),
                        );
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => anyhow::bail!("the agent exited during sign-in"),
                }
            }
        };
        tokio::time::timeout(wait, wait_for_auth)
            .await
            .map_err(|_| anyhow::anyhow!("sign-in timed out"))?
    }
    .await;
    peer.out.close();
    process::terminate(&mut child, agent.kill_grace).await;
    result.map_err(|e| {
        let tail = stderr.snapshot();
        if tail.is_empty() {
            e
        } else {
            anyhow::anyhow!("{e}\n{tail}")
        }
    })
}

async fn drive(
    proc: AgentProcess,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<AgentEvent>,
    config: SessionConfig,
    agent: AcpAgent,
) {
    let AgentProcess {
        mut child,
        stdin,
        stdout,
        stderr,
    } = proc;
    let mut peer = RpcPeer::new(RpcOut::new(stdin), stdout);
    // `RpcPeer::on_text` is a plain fn pointer; Antigravity's prefix is the
    // only one in use.
    if agent.auth_url_prefix == Some(ANTIGRAVITY_AUTH_PREFIX) {
        peer.on_text = Some(|line| auth_url(ANTIGRAVITY_AUTH_PREFIX, line));
    }
    let setup_result = tokio::select! {
        r = setup(&mut peer, &config, &agent, &events) => r,
        _ = events.closed() => Err(String::new()),
    };
    let session_id = match setup_result {
        Ok(id) => id,
        Err(message) => {
            if let Some(url) = message.strip_prefix("AUTH:") {
                let _ = events
                    .send(AgentEvent::AuthRequired {
                        message: format!("Sign in to {} to continue.", agent.display_name),
                        url: Some(url.to_owned()),
                    })
                    .await;
            } else if !message.is_empty() {
                let tail = stderr.snapshot();
                let message = if tail.is_empty() {
                    message
                } else {
                    format!("{message}\n{tail}")
                };
                let _ = events.send(AgentEvent::Error { message }).await;
            }
            peer.out.close();
            process::terminate(&mut child, agent.kill_grace).await;
            return;
        }
    };
    if events
        .send(AgentEvent::SessionStarted {
            provider_session_id: session_id.clone(),
        })
        .await
        .is_err()
    {
        process::terminate(&mut child, agent.kill_grace).await;
        return;
    }
    let mut driver = Driver::new(session_id, agent.sanitize_updates);
    let mut batch = Vec::new();
    'main: loop {
        tokio::select! {
            msg = peer.next() => match msg {
                Ok(Some(Incoming::Text(line))) => {
                    // A sign-in URL mid-session (token expired).
                    if let Some(prefix) = agent.auth_url_prefix
                        && let Some(url) = auth_url(prefix, &line)
                    {
                        batch.push(AgentEvent::AuthRequired {
                            message: format!("Sign in to {} to continue.", agent.display_name),
                            url: Some(url),
                        });
                    }
                }
                Ok(Some(msg)) => driver.incoming(&mut peer.out, msg, &mut batch),
                Ok(None) | Err(_) => {
                    if driver.turn_active() {
                        let name = agent.display_name;
                        batch.push(AgentEvent::Error {
                            message: process::crash_message(name, &mut child, &stderr),
                        });
                        batch.push(AgentEvent::TurnCompleted { status: TurnStatus::Failed });
                    }
                    for event in batch.drain(..) {
                        let _ = events.send(event).await;
                    }
                    break 'main;
                }
            },
            command = commands.recv() => match command {
                Some(command) => driver.command(&mut peer.out, command),
                None => break 'main,
            },
        }
        for event in batch.drain(..) {
            if events.send(event).await.is_err() {
                break 'main;
            }
        }
    }
    peer.out.close();
    process::terminate(&mut child, agent.kill_grace).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::StdinMsg;

    fn harness() -> (Driver, RpcOut, mpsc::UnboundedReceiver<StdinMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Driver::new("s1".into(), true), RpcOut::new(tx), rx)
    }

    fn sent(rx: &mut mpsc::UnboundedReceiver<StdinMsg>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(StdinMsg::Line(line)) = rx.try_recv() {
            out.push(serde_json::from_str(&line).unwrap());
        }
        out
    }

    fn update(update: Value) -> Incoming {
        Incoming::Notification {
            method: "session/update".into(),
            params: json!({ "sessionId": "s1", "update": update }),
        }
    }

    #[test]
    fn prompt_tool_permission_turn() {
        let (mut d, mut rpc, mut rx) = harness();
        let mut out = Vec::new();
        d.command(&mut rpc, Command::Prompt("hi".into()));
        d.command(&mut rpc, Command::Prompt("queued".into()));
        let wire = sent(&mut rx);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["method"], "session/prompt");
        assert_eq!(wire[0]["params"]["prompt"][0]["text"], "hi");

        d.incoming(&mut rpc, update(json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"think"}})), &mut out);
        d.incoming(&mut rpc, update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}})), &mut out);
        d.incoming(&mut rpc, update(json!({"sessionUpdate":"tool_call","toolCallId":"t1","title":"Run command","kind":"execute","status":"pending","rawInput":{"CommandLine":"ls -la"}})), &mut out);
        d.incoming(
            &mut rpc,
            Incoming::Request {
                id: json!(0),
                method: "session/request_permission".into(),
                params: json!({"sessionId":"s1","toolCall":{"toolCallId":"t1","title":"Run command","rawInput":{"CommandLine":"ls -la"}},
                    "options":[{"optionId":"a1","name":"Allow","kind":"allow_once"},{"optionId":"a2","name":"Always","kind":"allow_always"},{"optionId":"r1","name":"Deny","kind":"reject_once"}]}),
            },
            &mut out,
        );
        d.command(
            &mut rpc,
            Command::Approve {
                request_id: "perm-1".into(),
                decision: ApprovalDecision::AllowForSession,
            },
        );
        assert_eq!(
            sent(&mut rx),
            vec![
                json!({"jsonrpc":"2.0","id":0,"result":{"outcome":{"outcome":"selected","optionId":"a2"}}})
            ]
        );
        d.incoming(&mut rpc, update(json!({"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","rawOutput":{"combinedOutput":"total 0","exitCode":0}})), &mut out);
        d.incoming(
            &mut rpc,
            Incoming::Response {
                id: 1,
                result: Ok(json!({"stopReason":"end_turn"})),
            },
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                AgentEvent::ReasoningDelta {
                    text: "think".into()
                },
                AgentEvent::TextDelta {
                    text: "Hello".into()
                },
                AgentEvent::ToolCall {
                    call_id: "t1".into(),
                    name: "Run command".into(),
                    input: json!({"CommandLine":"ls -la","command":"ls -la"})
                },
                AgentEvent::ApprovalRequest {
                    request_id: "perm-1".into(),
                    title: "Run command".into(),
                    detail: "ls -la".into()
                },
                AgentEvent::ToolResult {
                    call_id: "t1".into(),
                    is_error: false,
                    output: "total 0".into(),
                    exit_code: Some(0),
                },
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed
                },
            ]
        );
        let wire = sent(&mut rx);
        assert_eq!(wire[0]["params"]["prompt"][0]["text"], "queued");
    }

    #[test]
    fn cancel_answers_permissions_and_ends_interrupted() {
        let (mut d, mut rpc, mut rx) = harness();
        let mut out = Vec::new();
        d.command(&mut rpc, Command::Prompt("hi".into()));
        d.incoming(
            &mut rpc,
            Incoming::Request {
                id: json!(3),
                method: "session/request_permission".into(),
                params: json!({"sessionId":"s1","toolCall":{"toolCallId":"interaction_1","title":"Pick"},
                    "options":[{"optionId":"x","name":"X","kind":"allow_once"}]}),
            },
            &mut out,
        );
        assert!(matches!(&out[0], AgentEvent::ApprovalRequest { detail, .. } if detail == "X"));
        d.command(&mut rpc, Command::Interrupt);
        let wire = sent(&mut rx);
        assert_eq!(
            wire[1],
            json!({"jsonrpc":"2.0","id":3,"result":{"outcome":{"outcome":"cancelled"}}})
        );
        assert_eq!(
            wire[2],
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s1"}})
        );
        d.incoming(
            &mut rpc,
            Incoming::Response {
                id: 1,
                result: Ok(json!({"stopReason":"cancelled"})),
            },
            &mut out,
        );
        assert_eq!(
            out.last(),
            Some(&AgentEvent::TurnCompleted {
                status: TurnStatus::Interrupted
            })
        );
    }

    #[test]
    fn sanitizes_large_and_image_payloads() {
        let big = "x".repeat(20_000);
        let mut u = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t",
            "title": big,
            "rawOutput": {"combinedOutput": big, "formattedOutput": big, "img": "data:image/png;base64,AAAA"},
            "content": [{"type":"content","content":{"type":"image","data":"AAAA","mimeType":"image/png"}}],
        });
        sanitize_update(&mut u);
        let title = u["title"].as_str().unwrap();
        assert!(
            title.starts_with(TRUNCATED_MARK)
                && title.len() <= TOOL_TEXT_LIMIT + TRUNCATED_MARK.len()
        );
        assert!(u["rawOutput"].get("formattedOutput").is_none());
        assert!(u["rawOutput"].get("img").is_none());
        assert!(u["content"][0]["content"].get("data").is_none());
        // Non-tool updates are untouched.
        let mut chunk =
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text": big}});
        sanitize_update(&mut chunk);
        assert_eq!(chunk["content"]["text"].as_str().unwrap().len(), 20_000);
    }

    #[test]
    fn auth_line_and_registry_manifest() {
        assert_eq!(
            auth_url(
                ANTIGRAVITY_AUTH_PREFIX,
                "Open the following link to authenticate the ACP server: https://accounts.google.com/o?x=1"
            ),
            Some("https://accounts.google.com/o?x=1".into())
        );
        assert_eq!(
            auth_url(ANTIGRAVITY_AUTH_PREFIX, "{\"jsonrpc\":\"2.0\"}"),
            None
        );
        let manifest = json!({
            "id": "antigravity-acp", "version": "1.3.0",
            "distribution": {"binary": {
                "linux-x86_64": {"archive": "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.3.0-linux-x86_64.zip", "cmd": "./agy_acp_server.par", "args": ["--uid="]},
                "darwin-aarch64": {"archive": "https://evil.example/x.zip", "cmd": "./agy_acp_server.par"}
            }}
        });
        assert_eq!(
            release_from_manifest(&manifest, "linux-x86_64"),
            Some(Release {
                version: "1.3.0".into(),
                archive_url: "https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-1.3.0-linux-x86_64.zip".into(),
                cmd: "agy_acp_server.par".into(),
                args: vec!["--uid=".into()],
            })
        );
        assert_eq!(release_from_manifest(&manifest, "darwin-aarch64"), None);
    }
}
