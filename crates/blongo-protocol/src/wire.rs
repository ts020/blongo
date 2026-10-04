//! Blongo's own client ↔ server protocol (`blongo serve`).
//!
//! - **Framing**: one frame = one MessagePack document. Over WebSocket a
//!   frame is one binary message; over a byte stream (SSH stdio, the local
//!   Unix socket) it is prefixed with its length as a big-endian `u32`
//!   ([`write_frame`] / [`FrameReader`]). Frames are bounded:
//!   [`MAX_CLIENT_FRAME`] client → server, [`MAX_SERVER_FRAME`] the other
//!   way.
//! - **Handshake**: `Hello` (version range, capabilities, optional resume
//!   point) → `Challenge` (negotiated version and capabilities, the
//!   server's id, a fresh nonce, the stream epoch) → `Auth` → `Welcome` or
//!   `Refused`. See [`proof_message`] for the proof of possession.
//! - **Stream**: every state message carries a sequence number
//!   ([`Sequenced`]), strictly increasing within one server `epoch`. A
//!   client that reconnects sends `(epoch, last_seq)`; the server replays
//!   exactly the messages after `last_seq` (no gap, no duplicate) when it
//!   still has them, or sends fresh snapshots otherwise.
//! - Bodies travel inside the frame (the in-process types keep them out of
//!   their serde form on purpose, see [`crate::domain`]), so the wire types
//!   wrap items with their text.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::{ImportReport, InstallState, LoginState};
use crate::workspace::{Query, QueryId, QueryReply};
use crate::{
    CommandEnvelope, CommandId, DomainEvent, EventKind, ItemId, ModelInfo, ProviderKind, Run,
    RunStatus, ShellSnapshot, ThreadId, ThreadSnapshot, TurnItem,
};

/// Version this build speaks.
pub const PROTOCOL_VERSION: u16 = 3;
/// Oldest version this build still accepts (v3 added pull request links
/// and forge settings to the shared types and events, which a v2 peer
/// cannot decode).
pub const MIN_PROTOCOL_VERSION: u16 = 3;

/// Largest frame a client may send (commands carry user text).
pub const MAX_CLIENT_FRAME: usize = 1 << 20;
/// Largest frame a server sends (a snapshot of a long thread). Snapshots
/// larger than this are refused rather than sent.
pub const MAX_SERVER_FRAME: usize = 16 << 20;

/// Optional features, negotiated as the intersection of both sides' lists.
pub mod caps {
    /// Server-side terminals (`Terminal*` messages).
    pub const TERMINAL: &str = "terminal";
    /// Provider sign-in and the Antigravity install on the server.
    pub const PROVIDER_SETUP: &str = "provider-setup";
    /// t3code import on the server.
    pub const IMPORT: &str = "import";
    /// Workspace queries (diffs, files, git; `Query` / `Reply`).
    pub const QUERY: &str = "query";

    pub const ALL: [&str; 4] = [TERMINAL, PROVIDER_SETUP, IMPORT, QUERY];
}

/// How long a server remembers a proof's `jti` (defence in depth: every
/// proof also signs the connection's fresh nonce, which is what makes it
/// single-use; `iat` is signed but not checked against the server clock,
/// so clock skew between the machines never locks a client out).
pub const PROOF_REPLAY_WINDOW_SECS: u64 = 600;

// ------------------------------------------------------------------ client

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello(Hello),
    Auth(AuthRequest),
    Command(CommandEnvelope),
    /// The threads whose timelines the client shows (replaces the previous
    /// set). The server answers with a snapshot of each newly added one and
    /// then streams their item events.
    Subscribe {
        threads: Vec<ThreadId>,
    },
    Login {
        provider: ProviderKind,
    },
    InstallAntigravity,
    /// Import t3code's database on the server (`None`: its default path).
    ImportT3 {
        path: Option<String>,
    },
    Ping {
        at: u64,
    },
    TerminalOpen {
        id: u32,
        thread_id: ThreadId,
        columns: u16,
        lines: u16,
    },
    TerminalInput {
        id: u32,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    TerminalResize {
        id: u32,
        columns: u16,
        lines: u16,
    },
    TerminalClose {
        id: u32,
    },
    /// A workspace query, answered with [`ServerMsg::Reply`] (same id).
    Query {
        id: QueryId,
        query: Query,
    },
    /// Administration, accepted only on transports the operating system
    /// authenticated (the server's private Unix socket, SSH stdio): remove
    /// a paired device and close its live connections. Answered with
    /// [`ServerMsg::Revoked`].
    Revoke {
        device: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub min_version: u16,
    pub capabilities: Vec<String>,
    /// Free-form client name (logs only).
    pub client: String,
    pub resume: Option<Resume>,
}

impl Hello {
    pub fn new(client: impl Into<String>, resume: Option<Resume>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            min_version: MIN_PROTOCOL_VERSION,
            capabilities: caps::ALL.iter().map(|c| c.to_string()).collect(),
            client: client.into(),
            resume,
        }
    }
}

/// Where a reconnecting client left off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub epoch: u64,
    pub last_seq: u64,
    /// Threads whose timeline the client holds up to `last_seq` (it got
    /// their snapshot and every event since). Replayed events cover these.
    pub threads: Vec<ThreadId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthRequest {
    /// A paired device: a proof made with the key bound to it at pairing,
    /// over the SHA-256 of its bearer token. The token itself never
    /// crosses the wire after pairing.
    Token { device_id: String, proof: Proof },
    /// Trade a one-time pairing code for a long-lived credential bound to
    /// `public_key` (Ed25519). The code is not sent: the proof covers its
    /// SHA-256, which the server checks against each outstanding code.
    Pair {
        device_name: String,
        #[serde(with = "serde_bytes")]
        public_key: Vec<u8>,
        proof: Proof,
    },
    /// Transports whose peer the operating system already authenticated
    /// (SSH stdio, the server's private Unix socket). Refused elsewhere.
    Local,
}

/// Proof of possession of the device key (t3code's DPoP, reshaped for a
/// handshake): an Ed25519 signature over [`proof_message`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// Unix seconds when the proof was made.
    pub iat: u64,
    /// Unique per proof; the server remembers recent ones.
    #[serde(with = "serde_bytes")]
    pub jti: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofPurpose {
    Token,
    Pair,
}

/// The bytes a proof signs. They bind the proof to this server, this
/// connection's nonce, the time, a unique id, the credential presented
/// (`sha256(token)` or `sha256(code)`, never the secret itself) and the
/// device key, so a captured proof is useless on another connection,
/// server or credential.
///
/// It is not bound to the transport channel: an active man in the middle
/// who relays the server's challenge to the client and the client's proof
/// back (possible only where the link is unencrypted and unauthenticated,
/// e.g. `--insecure-listen` without a TLS proxy) is authenticated as that
/// device on its own connection. Loopback, Tailscale and SSH rule that out.
pub fn proof_message(
    purpose: ProofPurpose,
    server_id: &str,
    nonce: &[u8],
    iat: u64,
    jti: &[u8],
    secret_sha256: &[u8],
    public_key: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160 + server_id.len());
    out.extend_from_slice(b"blongo-proof-v1\0");
    out.push(match purpose {
        ProofPurpose::Token => 0,
        ProofPurpose::Pair => 1,
    });
    for part in [server_id.as_bytes(), nonce, jti, public_key] {
        out.extend_from_slice(&(part.len() as u32).to_be_bytes());
        out.extend_from_slice(part);
    }
    out.extend_from_slice(&iat.to_be_bytes());
    out.extend_from_slice(secret_sha256);
    out
}

/// SHA-256 of a proof's secret (token or normalized pairing code).
pub fn secret_sha256(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

// ------------------------------------------------------------------ server

/// One frame from the server: one or more messages (a writer drains its
/// queue into a single frame).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerFrame(pub Vec<ServerMsg>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Challenge(Challenge),
    Welcome(Welcome),
    Refused {
        code: RefuseCode,
        message: String,
    },
    /// State: snapshots, events, deltas. Applied in `seq` order.
    Seq(Sequenced),
    /// The server dropped this client's backlog (it read too slowly).
    /// Fresh snapshots follow; replace everything with them.
    Resnapshot,
    CommandRejected {
        command_id: CommandId,
        reason: String,
    },
    CommandDuplicate {
        command_id: CommandId,
    },
    RunFinished {
        thread_id: ThreadId,
        status: RunStatus,
    },
    Models {
        provider: ProviderKind,
        models: Vec<ModelInfo>,
    },
    Login {
        provider: ProviderKind,
        state: LoginState,
    },
    Install(InstallState),
    Notice {
        message: String,
    },
    Imported(Result<ImportReport, String>),
    /// The server's core stopped; no more commands are accepted.
    Failed {
        message: String,
    },
    Pong {
        at: u64,
    },
    TerminalOutput {
        id: u32,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    TerminalExited {
        id: u32,
    },
    TerminalFailed {
        id: u32,
        message: String,
    },
    /// Answer to [`ClientMsg::Revoke`]: devices removed (0: none matched),
    /// or why it failed.
    Revoked(Result<usize, String>),
    /// This connection's device was revoked; the server closes it.
    DeviceRevoked,
    Reply {
        id: QueryId,
        result: Result<QueryReply, String>,
    },
    /// Terminal input was dropped (the shell is not reading it).
    TerminalInputDropped {
        id: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Challenge {
    pub version: u16,
    pub capabilities: Vec<String>,
    pub server_id: String,
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    /// Identifies this server process's sequence space.
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The authenticated device (`None` for local transports).
    pub device_id: Option<String>,
    /// Pairing only: the long-lived credential to store.
    pub issued: Option<IssuedCredential>,
    /// The server replays from the client's `last_seq`; otherwise
    /// snapshots follow.
    pub resumed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCredential {
    pub device_id: String,
    pub token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefuseCode {
    /// No version both sides speak.
    Version,
    /// Bad or unknown credential, bad proof, expired or used pairing code.
    Unauthorized,
    /// The message made no sense at this point.
    Protocol,
    /// The server is shutting down or full.
    Unavailable,
}

impl RefuseCode {
    /// Whether retrying with the same credential can ever succeed.
    pub fn is_permanent(self) -> bool {
        matches!(self, Self::Version | Self::Unauthorized)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequenced {
    pub seq: u64,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Payload {
    Shell(ShellSnapshot),
    Thread(WireThread),
    Event(WireEvent),
    TextDelta {
        thread_id: ThreadId,
        item_id: ItemId,
        chunk: Arc<str>,
    },
}

impl Payload {
    /// The thread whose timeline this belongs to (`None`: everyone's
    /// sidebar needs it).
    pub fn timeline_thread(&self) -> Option<ThreadId> {
        match self {
            Self::Shell(_) => None,
            Self::Thread(t) => Some(t.thread_id),
            Self::TextDelta { thread_id, .. } => Some(*thread_id),
            Self::Event(e) => match &e.event.kind {
                EventKind::ItemAdded { item } | EventKind::ItemUpdated { item } => {
                    Some(item.thread_id)
                }
                EventKind::ItemFinished { thread_id, .. }
                | EventKind::ItemTextAppended { thread_id, .. } => Some(*thread_id),
                _ => None,
            },
        }
    }

    /// Rough encoded size, for bounding queues without encoding.
    pub fn approx_size(&self) -> usize {
        64 + match self {
            Self::Shell(s) => 256 * (s.projects.len() + s.threads.len()),
            Self::Thread(t) => t
                .items
                .iter()
                .map(|i| 160 + i.text.len() + kind_size(&i.item.kind))
                .sum::<usize>()
                .saturating_add(96 * t.runs.len()),
            Self::Event(e) => {
                128 + e.text.as_ref().map_or(0, |t| t.len())
                    + match &e.event.kind {
                        EventKind::ItemAdded { item } | EventKind::ItemUpdated { item } => {
                            kind_size(&item.kind)
                        }
                        _ => 64,
                    }
            }
            Self::TextDelta { chunk, .. } => 40 + chunk.len(),
        }
    }
}

fn kind_size(kind: &crate::ItemKind) -> usize {
    use crate::ItemKind::*;
    match kind {
        CommandExecution {
            command, output, ..
        } => command.len() + output.len(),
        FileChange { paths, .. } => paths.iter().map(|p| p.len() + 4).sum(),
        ToolCall {
            name,
            input,
            output,
            ..
        } => name.len() + input.len() + output.len(),
        ApprovalRequest { title, detail, .. } => title.len() + detail.len(),
        Plan { steps } => steps.iter().map(|s| s.text.len() + 8).sum(),
        SystemNotice { message } | Error { message } => message.len(),
        _ => 0,
    }
}

/// A domain event with the body its in-process form keeps out of serde.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireEvent {
    pub event: Arc<DomainEvent>,
    /// Body of the item of `item.added` / `item.updated`.
    pub text: Option<Arc<str>>,
}

impl WireEvent {
    pub fn new(event: Arc<DomainEvent>) -> Self {
        let text = match &event.kind {
            EventKind::ItemAdded { item } | EventKind::ItemUpdated { item }
                if !item.text.is_empty() =>
            {
                Some(item.text.clone())
            }
            _ => None,
        };
        Self { event, text }
    }

    /// Back to the in-process form, body restored.
    pub fn into_event(self) -> DomainEvent {
        let Self { event, text } = self;
        let mut event = Arc::unwrap_or_clone(event);
        if let Some(text) = text
            && let EventKind::ItemAdded { item } | EventKind::ItemUpdated { item } = &mut event.kind
        {
            Arc::make_mut(item).text = text;
        }
        event
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireItem {
    pub item: TurnItem,
    pub text: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireThread {
    pub thread_id: ThreadId,
    pub sequence: u64,
    pub runs: Vec<Run>,
    pub items: Vec<WireItem>,
}

impl From<&ThreadSnapshot> for WireThread {
    fn from(s: &ThreadSnapshot) -> Self {
        Self {
            thread_id: s.thread_id,
            sequence: s.sequence,
            runs: s.runs.clone(),
            items: s
                .items
                .iter()
                .map(|i| WireItem {
                    item: TurnItem::clone(i),
                    text: i.text.clone(),
                })
                .collect(),
        }
    }
}

impl From<WireThread> for ThreadSnapshot {
    fn from(w: WireThread) -> Self {
        Self {
            thread_id: w.thread_id,
            sequence: w.sequence,
            runs: w.runs,
            items: w
                .items
                .into_iter()
                .map(|WireItem { mut item, text }| {
                    item.text = text;
                    Arc::new(item)
                })
                .collect(),
        }
    }
}

// ------------------------------------------------------------------- codec

#[derive(Debug)]
pub enum WireError {
    TooLarge { len: usize, max: usize },
    Encode(String),
    Decode(String),
    Io(std::io::Error),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { len, max } => write!(f, "frame of {len} bytes exceeds {max}"),
            Self::Encode(e) => write!(f, "cannot encode frame: {e}"),
            Self::Decode(e) => write!(f, "malformed frame: {e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<std::io::Error> for WireError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Encode one value as a frame payload (MessagePack, structs as maps so
/// the internally tagged domain enums round-trip).
pub fn encode<T: Serialize>(value: &T, max: usize) -> Result<Vec<u8>, WireError> {
    let bytes = rmp_serde::to_vec_named(value).map_err(|e| WireError::Encode(e.to_string()))?;
    if bytes.len() > max {
        return Err(WireError::TooLarge {
            len: bytes.len(),
            max,
        });
    }
    Ok(bytes)
}

pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8], max: usize) -> Result<T, WireError> {
    if bytes.len() > max {
        return Err(WireError::TooLarge {
            len: bytes.len(),
            max,
        });
    }
    rmp_serde::from_slice(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

/// Length-prefix a payload for a byte-stream transport.
pub fn length_prefixed(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Incremental reader of length-prefixed frames. Its buffer never grows
/// past `max` plus one read.
pub struct FrameReader {
    buf: Vec<u8>,
    max: usize,
}

impl FrameReader {
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, if any. An oversized length is an error
    /// before its body is buffered.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, WireError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
        if len > self.max {
            return Err(WireError::TooLarge { len, max: self.max });
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let frame = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        if self.buf.capacity() > 2 * self.max.min(1 << 20) && self.buf.is_empty() {
            self.buf = Vec::new();
        }
        Ok(Some(frame))
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Pick the version both sides speak (the highest common one).
pub fn negotiate_version(hello: &Hello) -> Option<u16> {
    let high = hello.version.min(PROTOCOL_VERSION);
    let low = hello.min_version.max(MIN_PROTOCOL_VERSION);
    (low <= high).then_some(high)
}

/// Capabilities both sides support, in this build's order.
pub fn negotiate_caps(offered: &[String]) -> Vec<String> {
    caps::ALL
        .iter()
        .filter(|c| offered.iter().any(|o| o == *c))
        .map(|c| c.to_string())
        .collect()
}

#[cfg(test)]
mod tests;
