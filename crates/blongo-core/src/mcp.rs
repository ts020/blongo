//! Blongo's MCP server for agents: `t3_thread_*`, `delegate_task` and the
//! scheduled-task tools, in a hand-written JSON-RPC 2.0 (newline-delimited,
//! MCP stdio framing). No SDK.
//!
//! ```text
//! agent ──stdio──▶ blongo mcp-bridge SOCKET TOKEN_FILE ──Unix socket──▶ core
//! ```
//!
//! - The core listens on `<data_dir>/mcp/s` (folder 0700, socket 0600).
//! - Each agent session gets its own random token, written to a 0600 file
//!   next to the socket; the agent's MCP config names the bridge, the
//!   socket and that file (never the token itself, so it is not in `ps`).
//!   The bridge sends the token as its first line, then copies bytes both
//!   ways.
//! - The token maps to the session's thread. Every tool runs **as that
//!   thread**, inside its project: threads of other projects do not exist
//!   for it (they answer "not found"). Archived threads are not listed.
//!   `delegate_task` children are capped in depth and number.
//! - Tokens die with their session (released, idle-stopped, or the core
//!   stopping); the files are removed then, and the folder at start.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use blongo_protocol::ThreadId;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Longest JSON-RPC line accepted from an agent.
const MAX_LINE: usize = 1 << 20;
/// Longest a waiting tool (`t3_thread_wait`, `delegate_task` in wait mode)
/// may block.
pub const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_WAIT: Duration = Duration::from_secs(10 * 60);
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// One tool call, for the orchestrator to answer.
pub(crate) struct McpCall {
    pub thread_id: ThreadId,
    pub tool: String,
    pub args: Value,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

pub(crate) struct McpServer {
    pub socket: PathBuf,
    dir: PathBuf,
    tokens: Arc<Mutex<HashMap<String, ThreadId>>>,
}

impl McpServer {
    /// Bind `<data_dir>/mcp/s` and accept bridges. `None` (logged) when
    /// the socket cannot be made (path too long, no Unix sockets).
    pub fn start(data_dir: &Path, calls: mpsc::UnboundedSender<McpCall>) -> Option<Self> {
        let dir = data_dir.join("mcp");
        match bind(&dir) {
            Ok((socket, listener)) => {
                let tokens: Arc<Mutex<HashMap<String, ThreadId>>> = Arc::default();
                tokio::spawn(accept(listener, tokens.clone(), calls));
                Some(Self {
                    socket,
                    dir,
                    tokens,
                })
            }
            Err(err) => {
                eprintln!("blongo-core: MCP server disabled: {err:#}");
                None
            }
        }
    }

    /// A new token for `thread_id`'s session. Dropping the grant revokes
    /// the token and removes its file.
    pub fn register(&self, thread_id: ThreadId) -> anyhow::Result<Grant> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let file = self.dir.join(format!("{}.token", &token[..16]));
        write_private(&file, token.as_bytes())?;
        self.tokens
            .lock()
            .expect("tokens")
            .insert(token.clone(), thread_id);
        Ok(Grant {
            tokens: self.tokens.clone(),
            token,
            file,
        })
    }
}

/// One session's access to the MCP server.
pub(crate) struct Grant {
    tokens: Arc<Mutex<HashMap<String, ThreadId>>>,
    token: String,
    pub file: PathBuf,
}

impl Drop for Grant {
    fn drop(&mut self) {
        self.tokens.lock().expect("tokens").remove(&self.token);
        let _ = std::fs::remove_file(&self.file);
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        self.tokens.lock().expect("tokens").clear();
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(unix)]
type Listener = tokio::net::UnixListener;
#[cfg(not(unix))]
type Listener = ();

#[cfg(unix)]
fn bind(dir: &Path) -> anyhow::Result<(PathBuf, Listener)> {
    use std::os::unix::fs::PermissionsExt;
    // Leftovers of an earlier process (tokens of sessions that are gone).
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = dir.join("s");
    if socket.as_os_str().len() >= 100 {
        anyhow::bail!("{} is too long for a Unix socket path", socket.display());
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    // The folder is private already; the socket too, for good measure.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    Ok((socket, listener))
}

#[cfg(not(unix))]
fn bind(_dir: &Path) -> anyhow::Result<(PathBuf, Listener)> {
    anyhow::bail!("the MCP bridge needs Unix sockets (not available on this platform yet)")
}

#[cfg(unix)]
async fn accept(
    listener: Listener,
    tokens: Arc<Mutex<HashMap<String, ThreadId>>>,
    calls: mpsc::UnboundedSender<McpCall>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let tokens = tokens.clone();
        let calls = calls.clone();
        tokio::spawn(async move {
            let (read, write) = stream.into_split();
            serve(BufReader::new(read), write, tokens, calls).await;
        });
    }
}

#[cfg(not(unix))]
async fn accept(
    _listener: Listener,
    _tokens: Arc<Mutex<HashMap<String, ThreadId>>>,
    _calls: mpsc::UnboundedSender<McpCall>,
) {
}

/// Read one line, at most [`MAX_LINE`] bytes. `None`: closed or too long.
async fn read_line<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Option<String> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await.ok()?;
        if available.is_empty() {
            return None;
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(i) => {
                buf.extend_from_slice(&available[..i]);
                reader.consume(i + 1);
                break;
            }
            None => {
                let n = available.len();
                buf.extend_from_slice(available);
                reader.consume(n);
            }
        }
        if buf.len() > MAX_LINE {
            return None;
        }
    }
    String::from_utf8(buf).ok()
}

async fn serve<R, W>(
    mut reader: R,
    mut writer: W,
    tokens: Arc<Mutex<HashMap<String, ThreadId>>>,
    calls: mpsc::UnboundedSender<McpCall>,
) where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    let Some(first) = read_line(&mut reader).await else {
        return;
    };
    let token = first.trim().strip_prefix("blongo-mcp ").unwrap_or("");
    let Some(thread_id) = tokens.lock().expect("tokens").get(token).copied() else {
        let _ = writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32001,\"message\":\"unknown session\"}}\n")
            .await;
        return;
    };
    // Responses from concurrent tool calls go through one writer.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    while let Some(line) = read_line(&mut reader).await {
        // Tokens die with their session: stop serving a released one.
        if !tokens.lock().expect("tokens").contains_key(token) {
            break;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let _ = out_tx.send(error_line(Value::Null, -32700, "parse error"));
            continue;
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            continue; // a response or garbage: nothing to answer
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification (initialized, cancelled)
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .filter(|v| PROTOCOL_VERSIONS.contains(v))
                    .unwrap_or(PROTOCOL_VERSIONS[0]);
                let _ = out_tx.send(result_line(
                    id,
                    json!({
                        "protocolVersion": asked,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "blongo", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS,
                    }),
                ));
            }
            "ping" => {
                let _ = out_tx.send(result_line(id, json!({})));
            }
            "tools/list" => {
                let _ = out_tx.send(result_line(id, json!({ "tools": tools() })));
            }
            "tools/call" => {
                let tool = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let wait = wait_budget(&tool, &args);
                let calls = calls.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    let (reply, rx) = oneshot::channel();
                    let sent = calls.send(McpCall {
                        thread_id,
                        tool,
                        args,
                        reply,
                    });
                    let result = match (sent, wait) {
                        (Err(_), _) => Err("Blongo is shutting down".to_owned()),
                        (Ok(()), Some(budget)) => match tokio::time::timeout(budget, rx).await {
                            Ok(r) => r.unwrap_or_else(|_| Err("Blongo stopped".into())),
                            Err(_) => Ok(json!({ "timedOut": true })),
                        },
                        (Ok(()), None) => rx.await.unwrap_or_else(|_| Err("Blongo stopped".into())),
                    };
                    let _ = out_tx.send(result_line(id, tool_result(result)));
                });
            }
            _ => {
                let _ = out_tx.send(error_line(id, -32601, &format!("unknown method {method}")));
            }
        }
    }
    drop(out_tx);
    let _ = writer_task.await;
}

/// How long a call may wait for a run to finish (`None`: no waiting).
fn wait_budget(tool: &str, args: &Value) -> Option<Duration> {
    let waits = match tool {
        "t3_thread_wait" => true,
        "delegate_task" => args.get("mode").and_then(Value::as_str) != Some("async"),
        _ => false,
    };
    waits.then(|| {
        args.get("timeoutMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_WAIT)
            .min(MAX_WAIT)
    })
}

fn result_line(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_line(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

/// MCP `CallToolResult`: failures are tool errors the model can read,
/// not protocol errors.
fn tool_result(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
            "structuredContent": value,
            "isError": false,
        }),
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    }
}

const INSTRUCTIONS: &str = "Blongo orchestration tools. You act as the Blongo thread this \
session belongs to and see only the threads of its project. Use delegate_task to hand one \
task to a child agent (mode=wait blocks until it finishes; mode=async returns at once, read \
it later with task_status). Use t3_thread_* for ordinary threads.";

fn tools() -> Value {
    let thread_id = json!({ "type": "string", "description": "A thread of this project (default: this thread)." });
    let provider = json!({ "type": "string", "enum": ["codex", "claude-code", "antigravity", "acp"],
                           "description": "Agent to use (default: this thread's)." });
    json!([
        {
            "name": "t3_thread_list",
            "description": "List the threads of the calling thread's project, newest first. Threads of other projects are never exposed.",
            "inputSchema": { "type": "object", "properties": {
                "status": { "type": "string", "enum": ["idle", "running", "waiting", "failed"] }
            } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "t3_thread_read",
            "description": "Read the latest messages (user and assistant) of a thread in this project, with its status. Each message is cut at 16,000 characters.",
            "inputSchema": { "type": "object", "properties": {
                "threadId": thread_id,
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Messages, newest last (default 20)." }
            } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "t3_thread_create",
            "description": "Create an ordinary top-level thread in this project (only when the user asked for a separate thread), optionally sending it a first message.",
            "inputSchema": { "type": "object", "properties": {
                "title": { "type": "string" },
                "message": { "type": "string" },
                "provider": provider
            } }
        },
        {
            "name": "t3_thread_send",
            "description": "Send a message to a thread in this project. mode=queue (default) starts an idle thread or queues behind its turn; mode=steer adds to the running turn.",
            "inputSchema": { "type": "object", "required": ["threadId", "message"], "properties": {
                "threadId": thread_id,
                "message": { "type": "string" },
                "mode": { "type": "string", "enum": ["queue", "steer"] }
            } }
        },
        {
            "name": "t3_thread_wait",
            "description": "Wait until a thread's running turn ends (an idle thread returns at once). Returns its status and last assistant message. timeoutMs (default 600000, max 1800000) does not interrupt the work.",
            "inputSchema": { "type": "object", "required": ["threadId"], "properties": {
                "threadId": thread_id,
                "timeoutMs": { "type": "integer", "minimum": 1 }
            } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "t3_thread_interrupt",
            "description": "Interrupt the running turn of a thread in this project.",
            "inputSchema": { "type": "object", "required": ["threadId"], "properties": { "threadId": thread_id } }
        },
        {
            "name": "delegate_task",
            "description": "Delegate one task to a Blongo child agent of THIS thread. It runs with only the given prompt (no copy of this conversation) in the same folder. mode=wait (default) blocks until it finishes and returns its final message; mode=async returns the child thread id at once (read it with task_status).",
            "inputSchema": { "type": "object", "required": ["prompt"], "properties": {
                "prompt": { "type": "string" },
                "title": { "type": "string" },
                "provider": provider,
                "mode": { "type": "string", "enum": ["wait", "async"] },
                "timeoutMs": { "type": "integer", "minimum": 1 }
            } }
        },
        {
            "name": "task_status",
            "description": "Status and (once finished) the final message of a task this thread delegated.",
            "inputSchema": { "type": "object", "required": ["childThreadId"], "properties": {
                "childThreadId": { "type": "string" }
            } },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "list_scheduled_tasks",
            "description": "List the recurring scheduled tasks of this project.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "schedule_task",
            "description": "Create a recurring task: at every time of the five-field cron expression (local time, e.g. '0 9 * * 1-5'), the prompt is sent to this thread (bindToCurrentThread, default true) or to a new thread of this project.",
            "inputSchema": { "type": "object", "required": ["cron", "prompt"], "properties": {
                "cron": { "type": "string" },
                "prompt": { "type": "string" },
                "bindToCurrentThread": { "type": "boolean" }
            } }
        }
    ])
}

/// `blongo mcp-bridge SOCKET TOKEN_FILE`: connect the agent's stdio to the
/// core's MCP socket. Returns the process exit code.
pub fn run_bridge(socket: &Path, token_file: &Path) -> i32 {
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        let token = match std::fs::read_to_string(token_file) {
            Ok(t) => t.trim().to_owned(),
            Err(e) => {
                eprintln!(
                    "blongo mcp-bridge: cannot read {}: {e}",
                    token_file.display()
                );
                return 2;
            }
        };
        let mut stream = match std::os::unix::net::UnixStream::connect(socket) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "blongo mcp-bridge: cannot connect to {}: {e}",
                    socket.display()
                );
                return 1;
            }
        };
        if stream
            .write_all(format!("blongo-mcp {token}\n").as_bytes())
            .is_err()
        {
            return 1;
        }
        let Ok(mut to_core) = stream.try_clone() else {
            return 1;
        };
        let upstream = std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 16 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if to_core.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = to_core.shutdown(std::net::Shutdown::Write);
        });
        let mut stdout = std::io::stdout().lock();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).is_err() || stdout.flush().is_err() {
                        break;
                    }
                }
            }
        }
        // The core closed: the agent's next write fails; do not wait for
        // stdin to end.
        drop(upstream);
        0
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, token_file);
        eprintln!("blongo mcp-bridge: needs Unix sockets (not available on this platform yet)");
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn protocol_round_trip_and_unknown_tokens() {
        let tokens: Arc<Mutex<HashMap<String, ThreadId>>> = Arc::default();
        let thread = ThreadId::new();
        tokens.lock().unwrap().insert("tok".into(), thread);
        let (calls_tx, mut calls_rx) = mpsc::unbounded_channel::<McpCall>();
        // The "orchestrator": answers every call with its tool name.
        tokio::spawn(async move {
            while let Some(call) = calls_rx.recv().await {
                assert_eq!(call.thread_id, thread);
                let _ = call.reply.send(match call.tool.as_str() {
                    "fail" => Err("nope".into()),
                    t => Ok(json!({ "tool": t, "args": call.args })),
                });
            }
        });
        let input = [
            "blongo-mcp tok",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"t3_thread_list","arguments":{"status":"idle"}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fail","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"nope"}"#,
            "not json",
        ]
        .join("\n")
            + "\n";
        let (client, server) = tokio::io::duplex(1 << 16);
        let (sr, sw) = tokio::io::split(server);
        let task = tokio::spawn(serve(BufReader::new(sr), sw, tokens.clone(), calls_tx));
        let (mut cr, mut cw) = tokio::io::split(client);
        cw.write_all(input.as_bytes()).await.unwrap();
        cw.shutdown().await.unwrap();
        task.await.unwrap();
        let mut out = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut cr, &mut out)
            .await
            .unwrap();
        let replies: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let by_id = |id: i64| replies.iter().find(|r| r["id"] == id).unwrap().clone();
        assert_eq!(by_id(1)["result"]["protocolVersion"], "2025-03-26");
        let names: Vec<String> = by_id(2)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        for expected in [
            "t3_thread_list",
            "t3_thread_send",
            "delegate_task",
            "task_status",
        ] {
            assert!(names.iter().any(|n| n == expected), "{expected}");
        }
        let call = by_id(3)["result"].clone();
        assert_eq!(call["isError"], false);
        assert_eq!(call["structuredContent"]["args"]["status"], "idle");
        assert_eq!(by_id(4)["result"]["isError"], true);
        assert_eq!(by_id(5)["error"]["code"], -32601);
        assert!(replies.iter().any(|r| r["error"]["code"] == -32700));

        // A wrong token gets one error line and nothing else.
        let (client, server) = tokio::io::duplex(4096);
        let (sr, sw) = tokio::io::split(server);
        let (calls_tx, _rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(serve(BufReader::new(sr), sw, tokens, calls_tx));
        let (mut cr, mut cw) = tokio::io::split(client);
        cw.write_all(
            b"blongo-mcp other\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n",
        )
        .await
        .unwrap();
        cw.shutdown().await.unwrap();
        task.await.unwrap();
        let mut out = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut cr, &mut out)
            .await
            .unwrap();
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("unknown session"));
    }

    #[test]
    fn waits_are_bounded() {
        assert_eq!(wait_budget("t3_thread_list", &json!({})), None);
        assert_eq!(
            wait_budget("delegate_task", &json!({"mode": "async"})),
            None
        );
        assert_eq!(wait_budget("delegate_task", &json!({})), Some(DEFAULT_WAIT));
        assert_eq!(
            wait_budget("t3_thread_wait", &json!({"timeoutMs": u64::MAX})),
            Some(MAX_WAIT)
        );
    }
}
