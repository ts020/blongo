//! Codex harness: `codex app-server` JSON-RPC 2.0 over stdio (the protocol
//! the Codex IDE extension speaks). Method and field names were checked
//! against `codex app-server generate-json-schema` from codex-cli 0.160.0.
//!
//! Flow (adapted from zeron crates/harness/src/codex/mod.rs):
//! `initialize` → `initialized` → `account/read` (sign-in check) →
//! `thread/start` → per prompt `turn/start`; notifications
//! `item/agentMessage/delta`, `item/reasoning/*Delta`, `item/started` /
//! `item/completed`, `turn/started`, `turn/completed`; server requests
//! `item/commandExecution/requestApproval` and
//! `item/fileChange/requestApproval`; interrupt is `turn/interrupt`.
//!
//! The npm `codex` command is a Node shim that spawns the native binary
//! (≈48 MB of extra RSS for an idle `node`). [`native_executable`] finds the
//! vendored native binary next to the shim so Blongo can skip Node.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use blongo_protocol::{AgentEvent, TurnStatus};
use serde_json::{Value, json};
use tokio::process::ChildStdout;
use tokio::sync::mpsc;

use crate::jsonrpc::{Incoming, RpcOut, RpcPeer};
use crate::process::{self, AgentProcess, StderrTail};
use crate::{ApprovalDecision, Command, Session, SessionConfig, preview_json};

pub const EXECUTABLE_ENV: &str = "BLONGO_CODEX_EXECUTABLE";

const SETUP_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TOOL_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct CodexOptions {
    /// `approvalPolicy`: `untrusted` | `on-request` | `never`.
    pub approval_policy: String,
    /// `sandbox`: `read-only` | `workspace-write` | `danger-full-access`.
    pub sandbox: String,
    /// Launch the vendored native binary instead of the npm Node shim.
    pub prefer_native: bool,
    pub kill_grace: Duration,
    /// Continue this Codex thread (`thread/resume`) instead of starting a
    /// new one. Falls back to `thread/start` when Codex cannot resume it;
    /// `SessionStarted` then carries a different id.
    pub resume_thread_id: Option<String>,
}

impl Default for CodexOptions {
    fn default() -> Self {
        Self {
            approval_policy: "on-request".into(),
            sandbox: "workspace-write".into(),
            prefer_native: true,
            kill_grace: Duration::from_secs(3),
            resume_thread_id: None,
        }
    }
}

/// Given the npm `codex` command (usually a symlink to
/// `<pkg>/bin/codex.js`), return the platform binary it would spawn.
pub fn native_executable(shim: &Path) -> Option<PathBuf> {
    let script = std::fs::canonicalize(shim).ok()?;
    if script.file_name()? != "codex.js" {
        return None;
    }
    let package_root = script.parent()?.parent()?;
    let (platform_pkg, triple) = platform_package()?;
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    [
        package_root
            .join("node_modules/@openai")
            .join(platform_pkg)
            .join("vendor"),
        package_root.join("vendor"),
    ]
    .into_iter()
    .map(|vendor| vendor.join(triple).join("bin").join(exe))
    .find(|p| p.is_file())
}

fn platform_package() -> Option<(&'static str, &'static str)> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("codex-linux-x64", "x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => ("codex-linux-arm64", "aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => ("codex-darwin-x64", "x86_64-apple-darwin"),
        ("macos", "aarch64") => ("codex-darwin-arm64", "aarch64-apple-darwin"),
        ("windows", "x86_64") => ("codex-win32-x64", "x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        _ => return None,
    })
}

/// Resolve the executable to launch: config / env / PATH, then (optionally)
/// swap the Node shim for the native binary.
pub fn resolve(config: &SessionConfig, options: &CodexOptions) -> std::io::Result<PathBuf> {
    let exe = process::resolve_executable(config.executable.as_deref(), EXECUTABLE_ENV, "codex")?;
    if options.prefer_native
        && let Some(native) = native_executable(&exe)
    {
        return Ok(native);
    }
    Ok(exe)
}

pub async fn start(config: SessionConfig) -> anyhow::Result<Session> {
    start_with(config, CodexOptions::default()).await
}

pub async fn start_with(config: SessionConfig, options: CodexOptions) -> anyhow::Result<Session> {
    let exe = resolve(&config, &options)?;
    let args: Vec<&OsStr> = vec![OsStr::new("app-server")];
    // What the npm shim would have set for the native binary.
    let proc = process::spawn(&exe, &args, &config, &[("CODEX_MANAGED_BY_NPM", "1")])?;
    let pid = proc.pid();
    let (event_tx, event_rx) = mpsc::channel(crate::EVENT_CHANNEL_CAPACITY);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let driver = tokio::spawn(drive(proc, cmd_rx, event_tx, config, options));
    Ok(Session::new(event_rx, cmd_tx, pid, driver))
}

pub(crate) fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "blongo", "title": "Blongo", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "experimentalApi": true },
    })
}

/// What a request we sent was for, keyed by its id.
#[derive(Debug, PartialEq)]
enum Pending {
    TurnStart,
    Interrupt,
}

/// Turn/notification state machine (no I/O except through `RpcOut`).
pub(crate) struct Driver {
    thread_id: String,
    pending: HashMap<i64, Pending>,
    turn_active: bool,
    turn_id: Option<String>,
    interrupt_requested: bool,
    queued_prompts: VecDeque<String>,
    /// JSON-RPC ids of approval requests awaiting the user, by event id.
    approvals: HashMap<String, Value>,
    /// agentMessage items that streamed deltas (completed-item fallback).
    streamed: HashSet<String>,
    options: CodexOptions,
    cwd: String,
    model: Option<String>,
}

impl Driver {
    pub(crate) fn new(thread_id: String, config: &SessionConfig, options: CodexOptions) -> Self {
        Self {
            thread_id,
            pending: HashMap::new(),
            turn_active: false,
            turn_id: None,
            interrupt_requested: false,
            queued_prompts: VecDeque::new(),
            approvals: HashMap::new(),
            streamed: HashSet::new(),
            options,
            cwd: config.cwd.to_string_lossy().into_owned(),
            model: config.model.clone(),
        }
    }

    pub(crate) fn command(&mut self, rpc: &mut RpcOut, command: Command) {
        match command {
            Command::Prompt(text) => {
                if self.turn_active {
                    self.queued_prompts.push_back(text);
                } else {
                    self.start_turn(rpc, &text);
                }
            }
            Command::Approve {
                request_id,
                decision,
            } => {
                if let Some(id) = self.approvals.remove(&request_id) {
                    let decision = match decision {
                        ApprovalDecision::Allow => "accept",
                        ApprovalDecision::AllowForSession => "acceptForSession",
                        ApprovalDecision::Deny => "decline",
                    };
                    rpc.respond(&id, json!({ "decision": decision }));
                }
            }
            Command::Interrupt => {
                if self.turn_active && !self.interrupt_requested {
                    self.interrupt_requested = true;
                    // Unanswered approvals would keep the turn blocked.
                    for (_, id) in self.approvals.drain() {
                        rpc.respond(&id, json!({ "decision": "cancel" }));
                    }
                    self.send_interrupt(rpc);
                }
            }
        }
    }

    fn start_turn(&mut self, rpc: &mut RpcOut, text: &str) {
        let mut params = json!({
            "threadId": self.thread_id,
            "input": [{ "type": "text", "text": text }],
            "approvalPolicy": self.options.approval_policy,
            "cwd": self.cwd,
            // Reasoning summaries only stream when asked for.
            "summary": "auto",
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        let id = rpc.request("turn/start", params);
        self.pending.insert(id, Pending::TurnStart);
        self.turn_active = true;
        self.turn_id = None;
        self.interrupt_requested = false;
    }

    fn send_interrupt(&mut self, rpc: &mut RpcOut) {
        // Without a turn id yet, `turn/started` (or the turn/start response)
        // sends it once the id is known.
        if let Some(turn_id) = &self.turn_id {
            let id = rpc.request(
                "turn/interrupt",
                json!({ "threadId": self.thread_id, "turnId": turn_id }),
            );
            self.pending.insert(id, Pending::Interrupt);
        }
    }

    fn note_turn_id(&mut self, rpc: &mut RpcOut, turn_id: Option<&str>) {
        if self.turn_id.is_none()
            && let Some(turn_id) = turn_id.filter(|t| !t.is_empty())
        {
            self.turn_id = Some(turn_id.to_owned());
            if self.interrupt_requested {
                self.send_interrupt(rpc);
            }
        }
    }

    fn finish_turn(&mut self, rpc: &mut RpcOut, status: TurnStatus, out: &mut Vec<AgentEvent>) {
        let status = if self.interrupt_requested && status != TurnStatus::Completed {
            TurnStatus::Interrupted
        } else {
            status
        };
        out.push(AgentEvent::TurnCompleted { status });
        self.turn_active = false;
        self.turn_id = None;
        self.interrupt_requested = false;
        self.streamed.clear();
        self.approvals.clear();
        if let Some(next) = self.queued_prompts.pop_front() {
            self.start_turn(rpc, &next);
        }
    }

    pub(crate) fn incoming(&mut self, rpc: &mut RpcOut, msg: Incoming, out: &mut Vec<AgentEvent>) {
        match msg {
            Incoming::Response { id, result } => match (self.pending.remove(&id), result) {
                (Some(Pending::TurnStart), Ok(result)) => {
                    self.note_turn_id(rpc, result.pointer("/turn/id").and_then(Value::as_str));
                }
                (Some(Pending::TurnStart), Err(e)) => {
                    out.push(AgentEvent::Error {
                        message: format!("turn/start: {e}"),
                    });
                    self.finish_turn(rpc, TurnStatus::Failed, out);
                }
                // After `cancel`-ed approvals the turn may already be over;
                // a late interrupt failure is then expected.
                (Some(Pending::Interrupt), Err(e)) if self.turn_active => {
                    out.push(AgentEvent::Error {
                        message: format!("turn/interrupt: {e}"),
                    })
                }
                _ => {}
            },
            Incoming::Notification { method, params } => {
                self.notification(rpc, &method, &params, out)
            }
            Incoming::Request { id, method, params } => {
                self.server_request(rpc, id, &method, &params, out)
            }
            Incoming::Text(_) => {}
        }
    }

    fn notification(
        &mut self,
        rpc: &mut RpcOut,
        method: &str,
        params: &Value,
        out: &mut Vec<AgentEvent>,
    ) {
        // Sub-agent threads share the connection; the spike follows only
        // its own thread.
        if let Some(thread) = params.get("threadId").and_then(Value::as_str)
            && thread != self.thread_id
        {
            return;
        }
        match method {
            "turn/started" => {
                self.note_turn_id(rpc, params.pointer("/turn/id").and_then(Value::as_str));
            }
            "item/agentMessage/delta" => {
                if let Some(item) = params.get("itemId").and_then(Value::as_str)
                    && !self.streamed.contains(item)
                {
                    self.streamed.insert(item.to_owned());
                }
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    out.push(AgentEvent::TextDelta { text: delta.into() });
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    out.push(AgentEvent::ReasoningDelta { text: delta.into() });
                }
            }
            "item/started" => {
                if let Some(event) = item_started(&params["item"]) {
                    out.push(event);
                }
            }
            "item/completed" => {
                let item = &params["item"];
                if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if !self.streamed.contains(id) && !text.is_empty() {
                        out.push(AgentEvent::TextDelta { text: text.into() });
                    }
                } else if let Some(event) = item_completed(item) {
                    out.push(event);
                }
            }
            "serverRequest/resolved" => {
                if let Some(id) = params.get("requestId") {
                    self.approvals.remove(&request_key(id));
                }
            }
            "error" => {
                // Retried transport errors are progress noise.
                if params.get("willRetry").and_then(Value::as_bool) != Some(true) {
                    let message = params
                        .pointer("/error/message")
                        .or_else(|| params.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("Codex error")
                        .to_owned();
                    out.push(AgentEvent::Error { message });
                }
            }
            "turn/completed" => {
                let turn = &params["turn"];
                let status = match turn.get("status").and_then(Value::as_str) {
                    Some("completed") => TurnStatus::Completed,
                    Some("interrupted") => TurnStatus::Interrupted,
                    _ => TurnStatus::Failed,
                };
                if let Some(message) = turn.pointer("/error/message").and_then(Value::as_str) {
                    out.push(AgentEvent::Error {
                        message: message.into(),
                    });
                }
                self.finish_turn(rpc, status, out);
            }
            _ => {}
        }
    }

    fn server_request(
        &mut self,
        rpc: &mut RpcOut,
        id: Value,
        method: &str,
        params: &Value,
        out: &mut Vec<AgentEvent>,
    ) {
        let (title, detail) = match method {
            "item/commandExecution/requestApproval" => {
                let command = params.get("command").and_then(Value::as_str).unwrap_or("");
                let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or("");
                let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
                let mut detail = crate::truncate(command, 4000);
                if !cwd.is_empty() {
                    detail.push_str(&format!("\n(in {cwd})"));
                }
                if !reason.is_empty() {
                    detail.push_str(&format!("\n{reason}"));
                }
                ("Run command?".to_owned(), detail)
            }
            "item/fileChange/requestApproval" => {
                let reason = params.get("reason").and_then(Value::as_str).unwrap_or("");
                let root = params
                    .get("grantRoot")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let detail = match (reason.is_empty(), root.is_empty()) {
                    (false, _) => reason.to_owned(),
                    (true, false) => format!("write access to {root}"),
                    (true, true) => preview_json(params.get("itemId").unwrap_or(&Value::Null), 200),
                };
                ("Apply file changes?".to_owned(), detail)
            }
            _ => {
                // requestUserInput / permissions / MCP elicitation / dynamic
                // tools are out of the spike's scope: refuse rather than
                // leave the agent blocked.
                rpc.respond_error(&id, -32601, &format!("unsupported by blongo: {method}"));
                return;
            }
        };
        let request_id = request_key(&id);
        out.push(AgentEvent::ApprovalRequest {
            request_id: request_id.clone(),
            title,
            detail,
        });
        self.approvals.insert(request_id, id);
    }
}

/// Stable string form of a JSON-RPC id (`0` → "0", `"a"` → "a").
fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn item_started(item: &Value) -> Option<AgentEvent> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => Some(AgentEvent::ToolCall {
            call_id: id,
            name: "shell".into(),
            input: json!({
                "command": item.get("command").cloned().unwrap_or(Value::Null),
                "cwd": item.get("cwd").cloned().unwrap_or(Value::Null),
            }),
        }),
        "fileChange" => Some(AgentEvent::ToolCall {
            call_id: id,
            name: "apply_patch".into(),
            input: json!({ "changes": item.get("changes").cloned().unwrap_or(Value::Null) }),
        }),
        "mcpToolCall" => Some(AgentEvent::ToolCall {
            call_id: id,
            name: format!(
                "{}/{}",
                item.get("server").and_then(Value::as_str).unwrap_or("mcp"),
                item.get("tool").and_then(Value::as_str).unwrap_or("tool")
            ),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        "webSearch" => Some(AgentEvent::ToolCall {
            call_id: id,
            name: "web_search".into(),
            input: json!({ "query": item.get("query").cloned().unwrap_or(Value::Null) }),
        }),
        _ => None,
    }
}

fn item_completed(item: &Value) -> Option<AgentEvent> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
    let failed = matches!(status, "failed" | "declined");
    match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => {
            let exit = item.get("exitCode").and_then(Value::as_i64);
            Some(AgentEvent::ToolResult {
                call_id: id,
                is_error: failed || exit.is_some_and(|c| c != 0),
                output: crate::truncate(
                    item.get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    MAX_TOOL_OUTPUT,
                ),
                exit_code: exit.and_then(|c| i32::try_from(c).ok()),
            })
        }
        "fileChange" | "webSearch" => Some(AgentEvent::ToolResult {
            call_id: id,
            is_error: failed,
            output: status.to_owned(),
            exit_code: None,
        }),
        "mcpToolCall" => Some(AgentEvent::ToolResult {
            call_id: id,
            is_error: failed || item.get("error").is_some_and(|e| !e.is_null()),
            output: preview_json(item.get("result").unwrap_or(&Value::Null), MAX_TOOL_OUTPUT),
            exit_code: None,
        }),
        _ => None,
    }
}

/// initialize → initialized → account/read → thread/start.
async fn setup(
    peer: &mut RpcPeer<ChildStdout>,
    config: &SessionConfig,
    options: &CodexOptions,
    events: &mpsc::Sender<AgentEvent>,
) -> Result<String, String> {
    peer.call("initialize", initialize_params(), SETUP_TIMEOUT)
        .await
        .map_err(|e| format!("initialize: {e}"))?;
    peer.out.notify("initialized", None);
    // Sign-in check. An API key in the environment also works, so this only
    // informs; thread/start still proceeds.
    if let Ok(account) = peer.call("account/read", json!({}), SETUP_TIMEOUT).await
        && account.get("account").is_some_and(Value::is_null)
        && account.get("requiresOpenaiAuth").and_then(Value::as_bool) == Some(true)
    {
        let _ = events
            .send(AgentEvent::AuthRequired {
                message: "Codex is not signed in (run `codex login`).".into(),
                url: None,
            })
            .await;
    }
    let mut params = json!({
        "cwd": config.cwd.to_string_lossy(),
        "approvalPolicy": options.approval_policy,
        "sandbox": options.sandbox,
    });
    if let Some(model) = &config.model {
        params["model"] = json!(model);
    }
    if let Some(resume) = &options.resume_thread_id {
        let mut resume_params = params.clone();
        resume_params["threadId"] = json!(resume);
        // Only the thread is needed, not its history: without
        // `excludeTurns` Codex sends every past turn in one (multi-MB) line.
        resume_params["excludeTurns"] = json!(true);
        // A thread Codex no longer has (or an older CLI) is not fatal: the
        // conversation continues in a fresh Codex thread.
        if let Ok(thread) = peer
            .call("thread/resume", resume_params, SETUP_TIMEOUT)
            .await
            && let Some(id) = thread.pointer("/thread/id").and_then(Value::as_str)
        {
            return Ok(id.to_owned());
        }
    }
    let thread = peer
        .call("thread/start", params, SETUP_TIMEOUT)
        .await
        .map_err(|e| format!("thread/start: {e}"))?;
    thread
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "thread/start returned no thread id".to_owned())
}

async fn drive(
    proc: AgentProcess,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::Sender<AgentEvent>,
    config: SessionConfig,
    options: CodexOptions,
) {
    let AgentProcess {
        mut child,
        stdin,
        stdout,
        stderr,
    } = proc;
    let kill_grace = options.kill_grace;
    let mut peer = RpcPeer::new(RpcOut::new(stdin), stdout);
    let thread_id = tokio::select! {
        result = setup(&mut peer, &config, &options, &events) => result,
        // Shutdown while still starting.
        _ = events.closed() => Err(String::new()),
    };
    let thread_id = match thread_id {
        Ok(id) => id,
        Err(message) => {
            if !message.is_empty() {
                let message = with_stderr(message, &stderr);
                let _ = events.send(AgentEvent::Error { message }).await;
            }
            peer.out.close();
            process::terminate(&mut child, kill_grace).await;
            return;
        }
    };
    if events
        .send(AgentEvent::SessionStarted {
            provider_session_id: thread_id.clone(),
        })
        .await
        .is_err()
    {
        process::terminate(&mut child, kill_grace).await;
        return;
    }
    let mut driver = Driver::new(thread_id, &config, options);
    let mut batch = Vec::new();
    'main: loop {
        tokio::select! {
            msg = peer.next() => match msg {
                Ok(Some(msg)) => driver.incoming(&mut peer.out, msg, &mut batch),
                Ok(None) | Err(_) => {
                    if driver.turn_active {
                        let message = process::crash_message("codex app-server", &mut child, &stderr);
                        batch.push(AgentEvent::Error { message });
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
    process::terminate(&mut child, kill_grace).await;
}

fn with_stderr(message: String, stderr: &StderrTail) -> String {
    let tail = stderr.snapshot();
    if tail.is_empty() {
        message
    } else {
        format!("{message}\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::StdinMsg;

    fn harness() -> (Driver, RpcOut, mpsc::UnboundedReceiver<StdinMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let driver = Driver::new(
            "th1".into(),
            &SessionConfig::new("/w"),
            CodexOptions::default(),
        );
        (driver, RpcOut::new(tx), rx)
    }

    fn sent(rx: &mut mpsc::UnboundedReceiver<StdinMsg>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(StdinMsg::Line(line)) = rx.try_recv() {
            out.push(serde_json::from_str(&line).unwrap());
        }
        out
    }

    fn note(method: &str, params: Value) -> Incoming {
        Incoming::Notification {
            method: method.into(),
            params,
        }
    }

    #[test]
    fn turn_with_approval_and_queue() {
        let (mut d, mut rpc, mut rx) = harness();
        let mut out = Vec::new();
        d.command(&mut rpc, Command::Prompt("one".into()));
        d.command(&mut rpc, Command::Prompt("two".into()));
        let wire = sent(&mut rx);
        assert_eq!(wire.len(), 1, "second prompt is queued");
        assert_eq!(wire[0]["method"], "turn/start");
        assert_eq!(wire[0]["params"]["input"][0]["text"], "one");
        assert_eq!(wire[0]["params"]["approvalPolicy"], "on-request");

        d.incoming(
            &mut rpc,
            note(
                "turn/started",
                json!({"threadId":"th1","turn":{"id":"tu1"}}),
            ),
            &mut out,
        );
        d.incoming(
            &mut rpc,
            note(
                "item/agentMessage/delta",
                json!({"threadId":"th1","itemId":"m1","delta":"Hel"}),
            ),
            &mut out,
        );
        d.incoming(
            &mut rpc,
            note(
                "item/agentMessage/delta",
                json!({"threadId":"other","itemId":"x","delta":"sub"}),
            ),
            &mut out,
        );
        d.incoming(
            &mut rpc,
            note(
                "item/completed",
                json!({"threadId":"th1","item":{"type":"agentMessage","id":"m1","text":"Hel"}}),
            ),
            &mut out,
        );
        d.incoming(&mut rpc, note("item/started", json!({"threadId":"th1","item":{"type":"commandExecution","id":"c1","command":"ls","cwd":"/w","status":"inProgress"}})), &mut out);
        d.incoming(
            &mut rpc,
            Incoming::Request {
                id: json!(0),
                method: "item/commandExecution/requestApproval".into(),
                params: json!({"threadId":"th1","turnId":"tu1","itemId":"c1","command":"ls","cwd":"/w","startedAtMs":1}),
            },
            &mut out,
        );
        d.command(
            &mut rpc,
            Command::Approve {
                request_id: "0".into(),
                decision: ApprovalDecision::AllowForSession,
            },
        );
        let wire = sent(&mut rx);
        assert_eq!(
            wire,
            vec![json!({"jsonrpc":"2.0","id":0,"result":{"decision":"acceptForSession"}})]
        );

        d.incoming(&mut rpc, note("item/completed", json!({"threadId":"th1","item":{"type":"commandExecution","id":"c1","status":"completed","exitCode":0,"aggregatedOutput":"a\n"}})), &mut out);
        d.incoming(
            &mut rpc,
            note(
                "turn/completed",
                json!({"threadId":"th1","turn":{"id":"tu1","status":"completed","error":null}}),
            ),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                AgentEvent::TextDelta { text: "Hel".into() },
                AgentEvent::ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: json!({"command":"ls","cwd":"/w"})
                },
                AgentEvent::ApprovalRequest {
                    request_id: "0".into(),
                    title: "Run command?".into(),
                    detail: "ls\n(in /w)".into()
                },
                AgentEvent::ToolResult {
                    call_id: "c1".into(),
                    is_error: false,
                    output: "a\n".into(),
                    exit_code: Some(0),
                },
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed
                },
            ]
        );
        // The queued prompt starts as the next turn.
        let wire = sent(&mut rx);
        assert_eq!(wire[0]["method"], "turn/start");
        assert_eq!(wire[0]["params"]["input"][0]["text"], "two");
    }

    #[test]
    fn interrupt_waits_for_turn_id_and_cancels_approvals() {
        let (mut d, mut rpc, mut rx) = harness();
        let mut out = Vec::new();
        d.command(&mut rpc, Command::Prompt("go".into()));
        d.incoming(
            &mut rpc,
            Incoming::Request {
                id: json!("a7"),
                method: "item/fileChange/requestApproval".into(),
                params: json!({"threadId":"th1","itemId":"f1","reason":"edit main.rs"}),
            },
            &mut out,
        );
        d.command(&mut rpc, Command::Interrupt);
        let wire = sent(&mut rx);
        assert_eq!(
            wire.len(),
            2,
            "turn/start + approval cancel, no interrupt yet"
        );
        assert_eq!(wire[1]["result"]["decision"], "cancel");
        d.incoming(
            &mut rpc,
            Incoming::Response {
                id: 1,
                result: Ok(json!({"turn":{"id":"tu9"}})),
            },
            &mut out,
        );
        let wire = sent(&mut rx);
        assert_eq!(wire[0]["method"], "turn/interrupt");
        assert_eq!(wire[0]["params"], json!({"threadId":"th1","turnId":"tu9"}));
        d.incoming(
            &mut rpc,
            note(
                "turn/completed",
                json!({"threadId":"th1","turn":{"id":"tu9","status":"interrupted"}}),
            ),
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
    fn failed_turn_start_and_unsupported_requests() {
        let (mut d, mut rpc, mut rx) = harness();
        let mut out = Vec::new();
        d.command(&mut rpc, Command::Prompt("go".into()));
        d.incoming(
            &mut rpc,
            Incoming::Request {
                id: json!(5),
                method: "item/tool/requestUserInput".into(),
                params: json!({}),
            },
            &mut out,
        );
        d.incoming(
            &mut rpc,
            Incoming::Response {
                id: 1,
                result: Err(crate::jsonrpc::RpcError {
                    code: -32600,
                    message: "nope".into(),
                    data: None,
                }),
            },
            &mut out,
        );
        assert_eq!(
            out[1],
            AgentEvent::TurnCompleted {
                status: TurnStatus::Failed
            }
        );
        let wire = sent(&mut rx);
        assert_eq!(wire[1]["error"]["code"], -32601);
        // Retried errors are swallowed, final ones surface.
        out.clear();
        d.incoming(&mut rpc, note("error", json!({"threadId":"th1","willRetry":true,"error":{"message":"Reconnecting... 2/5"}})), &mut out);
        d.incoming(&mut rpc, note("error", json!({"threadId":"th1","willRetry":false,"error":{"message":"stream disconnected"}})), &mut out);
        assert_eq!(
            out,
            vec![AgentEvent::Error {
                message: "stream disconnected".into()
            }]
        );
    }

    #[test]
    fn native_executable_ignores_non_shims() {
        assert_eq!(native_executable(Path::new("/bin/sh")), None);
    }
}
