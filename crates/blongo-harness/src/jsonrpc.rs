//! Newline-delimited JSON-RPC 2.0 over a child's stdio, shared by the Codex
//! app-server and ACP harnesses.
//!
//! Unlike zeron's client (crates/harness/src/jsonrpc.rs, which this is
//! adapted from) there is no reader task and no shared pending map: the
//! session driver owns the stdout reader and sees responses inline as
//! [`Incoming::Response`], matching them to what it asked for by id. Only
//! the setup phase uses the blocking-style [`RpcPeer::call`], which parks
//! anything else that arrives meanwhile in a small backlog.

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::process::{LineReader, StdinMsg};

/// One stdout line, classified.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Response {
        id: i64,
        result: Result<Value, RpcError>,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// Server→client request; must be answered.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A line that is not a JSON-RPC message (log noise, Antigravity's
    /// sign-in URL, …).
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)?;
        if let Some(data) = &self.data {
            match data.as_str() {
                Some(s) => write!(f, ": {s}")?,
                None => write!(f, ": {data}")?,
            }
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}

/// Tolerant id parse: servers may echo `5` as `"5"` or `5.0`.
fn numeric_id(id: &Value) -> Option<i64> {
    id.as_i64()
        .or_else(|| id.as_str().and_then(|s| s.parse().ok()))
        .or_else(|| id.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
}

/// Classify one line. `None` for blank lines and for JSON that is not a
/// JSON-RPC message we can act on.
pub fn parse_line(line: &str) -> Option<Incoming> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let Ok(mut msg) = serde_json::from_str::<Value>(line) else {
        return Some(Incoming::Text(line.to_owned()));
    };
    let obj = msg.as_object_mut()?;
    // Codex 0.160 omits `"jsonrpc"` on notifications; only reject a wrong one.
    if obj.get("jsonrpc").is_some_and(|v| v != "2.0") {
        return None;
    }
    let params = obj.remove("params").unwrap_or(Value::Null);
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    match (method, obj.remove("id")) {
        (Some(method), Some(id)) => Some(Incoming::Request { id, method, params }),
        (Some(method), None) => Some(Incoming::Notification { method, params }),
        (None, Some(id)) => {
            let id = numeric_id(&id)?;
            let result = if let Some(error) = obj.remove("error") {
                Err(RpcError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                    data: error.get("data").filter(|d| !d.is_null()).cloned(),
                })
            } else {
                Ok(obj.remove("result")?)
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}

/// The writing half: id allocation plus framing onto the stdin writer task.
pub struct RpcOut {
    next_id: i64,
    stdin: mpsc::UnboundedSender<StdinMsg>,
}

impl RpcOut {
    pub fn new(stdin: mpsc::UnboundedSender<StdinMsg>) -> Self {
        Self { next_id: 0, stdin }
    }

    /// Send a request; returns its id for matching the response.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    pub fn notify(&self, method: &str, params: Option<Value>) {
        let msg = match params {
            Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        self.send(msg);
    }

    pub fn respond(&self, id: &Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    pub fn respond_error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }));
    }

    pub fn close(&self) {
        let _ = self.stdin.send(StdinMsg::Close);
    }

    fn send(&self, msg: Value) {
        let _ = self.stdin.send(StdinMsg::Line(msg.to_string()));
    }
}

/// Errors from the setup-phase [`RpcPeer::call`].
#[derive(Debug)]
pub enum CallError {
    Rpc(RpcError),
    /// stdout closed before the response arrived.
    Eof,
    Timeout,
    Io(std::io::Error),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Rpc(e) => e.fmt(f),
            CallError::Eof => f.write_str("agent exited before responding"),
            CallError::Timeout => f.write_str("agent did not respond in time"),
            CallError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CallError {}

/// Reader + writer + backlog for one JSON-RPC peer.
pub struct RpcPeer<R> {
    pub out: RpcOut,
    pub reader: LineReader<R>,
    backlog: VecDeque<Incoming>,
    /// Hook for non-JSON lines seen during `call` (Antigravity's auth URL).
    pub on_text: Option<fn(&str) -> Option<String>>,
}

impl<R: tokio::io::AsyncRead + Unpin> RpcPeer<R> {
    pub fn new(out: RpcOut, reader: LineReader<R>) -> Self {
        Self {
            out,
            reader,
            backlog: VecDeque::new(),
            on_text: None,
        }
    }

    /// Drop buffered notifications (e.g. history replayed by `session/load`).
    pub fn drop_backlog_notifications(&mut self) {
        self.backlog
            .retain(|m| !matches!(m, Incoming::Notification { .. }));
    }

    /// Next message: backlog first, then stdout. `Ok(None)` at EOF.
    pub async fn next(&mut self) -> std::io::Result<Option<Incoming>> {
        if let Some(msg) = self.backlog.pop_front() {
            return Ok(Some(msg));
        }
        loop {
            match self.reader.next_line().await {
                Ok(Some(line)) => {
                    if let Some(msg) = parse_line(&line) {
                        return Ok(Some(msg));
                    }
                }
                Ok(None) => return Ok(None),
                // Over-long line: skip it, keep the session alive.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Request and wait for its response, parking everything else. Only for
    /// setup (initialize / thread start): the driver loop never calls this.
    /// A non-JSON line for which `on_text` returns `Some(msg)` aborts with
    /// that message as an RPC error (used for "sign-in required").
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let id = self.out.request(method, params);
        let wait = async {
            loop {
                let line = match self.reader.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => return Err(CallError::Eof),
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                    Err(e) => return Err(CallError::Io(e)),
                };
                match parse_line(&line) {
                    Some(Incoming::Response { id: got, result }) if got == id => {
                        return result.map_err(CallError::Rpc);
                    }
                    Some(Incoming::Text(text)) => {
                        if let Some(message) = self.on_text.and_then(|hook| hook(&text)) {
                            return Err(CallError::Rpc(RpcError {
                                code: AUTH_REQUIRED_CODE,
                                message,
                                data: None,
                            }));
                        }
                    }
                    Some(other) => self.backlog.push_back(other),
                    None => {}
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .unwrap_or(Err(CallError::Timeout))
    }
}

/// Synthetic error code for "the agent asked for a browser sign-in".
pub const AUTH_REQUIRED_CODE: i64 = -32_099;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert_eq!(
            parse_line(r#"{"id":"3","result":{"ok":true}}"#),
            Some(Incoming::Response {
                id: 3,
                result: Ok(json!({"ok": true}))
            })
        );
        assert_eq!(
            parse_line(r#"{"method":"turn/started","params":{"a":1},"emittedAtMs":1}"#),
            Some(Incoming::Notification {
                method: "turn/started".into(),
                params: json!({"a": 1})
            })
        );
        assert_eq!(
            parse_line(r#"{"jsonrpc":"2.0","id":0,"method":"x/y","params":{}}"#),
            Some(Incoming::Request {
                id: json!(0),
                method: "x/y".into(),
                params: json!({})
            })
        );
        let Some(Incoming::Response { result: Err(e), .. }) =
            parse_line(r#"{"id":1,"error":{"code":-32600,"message":"bad","data":"why"}}"#)
        else {
            panic!("error response")
        };
        assert_eq!(e.to_string(), "bad (code -32600): why");
        assert_eq!(
            parse_line("Loading…"),
            Some(Incoming::Text("Loading…".into()))
        );
        assert_eq!(parse_line(r#"{"jsonrpc":"1.0","method":"x"}"#), None);
        assert_eq!(parse_line("   "), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_parks_other_messages() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stdout: &[u8] = b"{\"method\":\"early\"}\nnoise\n{\"id\":1,\"result\":7}\n";
        let mut peer = RpcPeer::new(RpcOut::new(tx), LineReader::new(stdout));
        let got = peer
            .call("initialize", json!({}), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(got, json!(7));
        let Some(StdinMsg::Line(sent)) = rx.recv().await else {
            panic!()
        };
        let sent: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent["method"], "initialize");
        assert_eq!(sent["id"], 1);
        assert!(matches!(
            peer.next().await.unwrap(),
            Some(Incoming::Notification { method, .. }) if method == "early"
        ));
        assert_eq!(peer.next().await.unwrap(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_aborts_on_auth_hook() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let stdout: &[u8] = b"Open this: https://x\n";
        let mut peer = RpcPeer::new(RpcOut::new(tx), LineReader::new(stdout));
        peer.on_text = Some(|line| line.strip_prefix("Open this: ").map(str::to_owned));
        let err = peer
            .call("authenticate", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CallError::Rpc(RpcError { code: AUTH_REQUIRED_CODE, ref message, .. }) if message == "https://x")
        );
    }
}
