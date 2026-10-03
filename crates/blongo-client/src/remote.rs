//! A remote environment: one supervised connection to a `blongo serve`.
//!
//! The supervisor task (on the shared network runtime, [`crate::net`])
//! connects, authenticates, and turns server frames into the same
//! [`CoreEvent`]s the in-process core sends. It keeps the stream's
//! position (`epoch`, `last_seq`) and the timelines it holds, so after a
//! lost connection it asks the server to resume from there: replayed
//! messages at or below `last_seq` are dropped (no duplicates), and the
//! server either replays everything after it (no gaps) or sends fresh
//! snapshots. A position is only resumed once this epoch's sidebar snapshot
//! was applied (a link lost between `Welcome` and the snapshot starts
//! fresh). Reconnects follow [`Backoff`]; a revoked device stops for good. Commands that were sent but
//! not yet answered are re-sent after a reconnect; their ids make that
//! idempotent on the server.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use blongo_protocol::client::{ConnectionState, CoreEvent, TerminalEvent};
use blongo_protocol::wire::{ClientMsg, Hello, Payload, Resume, ServerMsg};
use blongo_protocol::{CommandEnvelope, CommandId, ProviderKind, ThreadId, ThreadSnapshot};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::backend::{Backend, Events};
use crate::backoff::Backoff;
use crate::environments::Environment;
use crate::handshake::{ClientAuth, HandshakeError, handshake, recv, send};
use crate::target::{Target, open};

/// Ping when nothing was sent, or nothing heard, for this long (the server
/// closes connections silent for 75 s).
const PING_EVERY: Duration = Duration::from_secs(20);
/// Give up on a connection that sent nothing for this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
/// Commands kept for re-sending after a reconnect.
const MAX_PENDING: usize = 64;
const PENDING_TTL: Duration = Duration::from_secs(120);

enum Op {
    Command(CommandEnvelope),
    OpenThread(ThreadId),
    Login(ProviderKind),
    InstallAntigravity,
    ImportT3(Option<String>),
    TerminalOpen {
        id: u32,
        thread_id: ThreadId,
        columns: u16,
        lines: u16,
    },
    TerminalInput(u32, Vec<u8>),
    TerminalResize(u32, u16, u16),
    TerminalClose(u32),
}

/// Handle to a remote environment. Dropping every clone stops the
/// connection.
#[derive(Clone)]
pub struct RemoteBackend {
    ops: mpsc::UnboundedSender<Op>,
}

impl RemoteBackend {
    fn send(&self, op: Op) {
        let _ = self.ops.send(op);
    }
}

impl Backend for RemoteBackend {
    fn dispatch(&self, command: CommandEnvelope) {
        self.send(Op::Command(command));
    }

    fn open_thread(&self, thread_id: ThreadId) {
        self.send(Op::OpenThread(thread_id));
    }

    fn login(&self, provider: ProviderKind) {
        self.send(Op::Login(provider));
    }

    fn install_antigravity(&self) {
        self.send(Op::InstallAntigravity);
    }

    fn import_t3(&self, source: Option<PathBuf>) {
        self.send(Op::ImportT3(
            source.map(|p| p.to_string_lossy().into_owned()),
        ));
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn terminal_open(&self, id: u32, thread_id: ThreadId, columns: u16, lines: u16) {
        self.send(Op::TerminalOpen {
            id,
            thread_id,
            columns,
            lines,
        });
    }

    fn terminal_input(&self, id: u32, data: Vec<u8>) {
        self.send(Op::TerminalInput(id, data));
    }

    fn terminal_resize(&self, id: u32, columns: u16, lines: u16) {
        self.send(Op::TerminalResize(id, columns, lines));
    }

    fn terminal_close(&self, id: u32) {
        self.send(Op::TerminalClose(id));
    }
}

#[derive(Clone, Debug)]
pub struct RemoteOptions {
    /// Shown in the server's log.
    pub client_name: String,
    pub backoff: Backoff,
}

impl Default for RemoteOptions {
    fn default() -> Self {
        Self {
            client_name: format!("blongo {}", env!("CARGO_PKG_VERSION")),
            backoff: Backoff::new(),
        }
    }
}

/// Start the connection to `env` on the network runtime.
pub fn connect(env: Environment, options: RemoteOptions) -> (RemoteBackend, Events) {
    connect_on(&crate::net::handle(), env, options)
}

/// Like [`connect`], on a given runtime (tests).
pub fn connect_on(
    handle: &tokio::runtime::Handle,
    env: Environment,
    options: RemoteOptions,
) -> (RemoteBackend, Events) {
    let (ops_tx, ops_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let supervisor = Supervisor {
        env,
        options,
        events: events_tx,
        ops: ops_rx,
        stream: StreamState::default(),
        pending: VecDeque::new(),
        terminals: HashMap::new(),
    };
    handle.spawn(supervisor.run());
    (RemoteBackend { ops: ops_tx }, events_rx)
}

// ----------------------------------------------------------------- stream

/// Position in the server's stream and the timelines held. Pure state, so
/// the sequencing rules are testable without a network.
#[derive(Debug, Default)]
pub struct StreamState {
    pub epoch: Option<u64>,
    pub last_seq: u64,
    /// Threads whose snapshot was applied and whose events since are all
    /// here.
    pub ready: HashSet<ThreadId>,
    /// Threads the UI wants (the open timeline).
    pub desired: Vec<ThreadId>,
    /// This epoch's sidebar snapshot was applied: the position is worth
    /// resuming. Without it a resume would skip the snapshot for good.
    pub have_shell: bool,
    /// Messages dropped as duplicates (tests, diagnostics).
    pub duplicates: u64,
    /// The server revoked this device.
    pub revoked: bool,
}

impl StreamState {
    /// What to ask the server for when (re)connecting.
    pub fn resume(&self) -> Option<Resume> {
        if !self.have_shell {
            return None;
        }
        self.epoch.map(|epoch| Resume {
            epoch,
            last_seq: self.last_seq,
            threads: self.ready.iter().copied().collect(),
        })
    }

    /// The server answered the handshake. `resumed == false`: everything
    /// held is stale; snapshots follow.
    pub fn on_welcome(&mut self, epoch: u64, resumed: bool) {
        if !resumed {
            self.ready.clear();
            self.last_seq = 0;
            self.have_shell = false;
        }
        self.epoch = Some(epoch);
    }

    /// Threads to (re)subscribe after the handshake, if any.
    pub fn needs_subscribe(&self) -> bool {
        self.desired.iter().any(|t| !self.ready.contains(t))
            || self.ready.iter().any(|t| !self.desired.contains(t))
    }

    pub fn open(&mut self, thread_id: ThreadId) {
        self.desired = vec![thread_id];
        // A fresh snapshot is coming; until then the thread is not held.
        self.ready.clear();
    }

    /// Apply one server message; returns what the UI should see.
    pub fn apply(&mut self, msg: ServerMsg) -> Vec<CoreEvent> {
        match msg {
            ServerMsg::Seq(s) => {
                let snapshot = matches!(s.payload, Payload::Shell(_) | Payload::Thread(_));
                if !snapshot && s.seq <= self.last_seq {
                    self.duplicates += 1;
                    return vec![];
                }
                self.last_seq = self.last_seq.max(s.seq);
                match s.payload {
                    Payload::Shell(shell) => {
                        self.have_shell = true;
                        vec![CoreEvent::Shell(Arc::new(shell))]
                    }
                    Payload::Thread(t) => {
                        if !self.desired.contains(&t.thread_id) {
                            return vec![];
                        }
                        self.ready.insert(t.thread_id);
                        vec![CoreEvent::Thread(Arc::new(ThreadSnapshot::from(t)))]
                    }
                    payload => {
                        if let Some(t) = payload.timeline_thread()
                            && !self.ready.contains(&t)
                        {
                            return vec![];
                        }
                        match payload {
                            Payload::Event(e) => vec![CoreEvent::Event(Arc::new(e.into_event()))],
                            Payload::TextDelta {
                                thread_id,
                                item_id,
                                chunk,
                            } => vec![CoreEvent::TextDelta {
                                thread_id,
                                item_id,
                                chunk,
                            }],
                            _ => unreachable!("snapshots handled above"),
                        }
                    }
                }
            }
            ServerMsg::Resnapshot => {
                self.ready.clear();
                self.have_shell = false;
                vec![]
            }
            ServerMsg::DeviceRevoked => {
                self.revoked = true;
                vec![]
            }
            ServerMsg::CommandRejected { command_id, reason } => {
                vec![CoreEvent::CommandRejected { command_id, reason }]
            }
            ServerMsg::CommandDuplicate { command_id } => {
                vec![CoreEvent::CommandDuplicate { command_id }]
            }
            ServerMsg::RunFinished { thread_id, status } => {
                vec![CoreEvent::RunFinished { thread_id, status }]
            }
            ServerMsg::Models { provider, models } => vec![CoreEvent::Models {
                provider,
                models: models.into(),
            }],
            ServerMsg::Login { provider, state } => vec![CoreEvent::Login { provider, state }],
            ServerMsg::Install(state) => vec![CoreEvent::Install(state)],
            ServerMsg::Notice { message } => vec![CoreEvent::Notice { message }],
            ServerMsg::Imported(result) => vec![CoreEvent::Imported(result)],
            // The server is going away (shutdown, its core stopped): the
            // link drops next and is retried like any other loss.
            ServerMsg::Failed { message } => vec![CoreEvent::Notice {
                message: format!("the server stopped: {message}"),
            }],
            ServerMsg::TerminalOutput { id, data } => {
                vec![CoreEvent::Terminal(TerminalEvent::Output {
                    id,
                    data: data.into(),
                })]
            }
            ServerMsg::TerminalExited { id } => {
                vec![CoreEvent::Terminal(TerminalEvent::Exited { id })]
            }
            ServerMsg::TerminalFailed { id, message } => {
                vec![CoreEvent::Terminal(TerminalEvent::Failed { id, message })]
            }
            ServerMsg::Pong { .. }
            | ServerMsg::Revoked(_)
            | ServerMsg::Challenge(_)
            | ServerMsg::Welcome(_)
            | ServerMsg::Refused { .. } => vec![],
        }
    }
}

// ------------------------------------------------------------- supervisor

struct Supervisor {
    env: Environment,
    options: RemoteOptions,
    events: mpsc::UnboundedSender<CoreEvent>,
    ops: mpsc::UnboundedReceiver<Op>,
    stream: StreamState,
    /// Sent, not yet answered (an event carrying its id, a rejection or a
    /// duplicate notice).
    pending: VecDeque<(Instant, CommandEnvelope)>,
    /// Open server-side terminals (they end with the connection).
    terminals: HashMap<u32, ThreadId>,
}

enum Outcome {
    Shutdown,
    Permanent(String),
    Lost {
        error: String,
        connected_for: Option<Duration>,
    },
}

impl Supervisor {
    fn emit(&self, event: CoreEvent) {
        let _ = self.events.send(event);
    }

    fn status(&self, state: ConnectionState) {
        self.emit(CoreEvent::Connection(state));
    }

    async fn run(mut self) {
        let target = match self.env.target() {
            Ok(t) => t,
            Err(e) => {
                self.status(ConnectionState::Failed(e));
                return self.idle_until_dropped().await;
            }
        };
        let mut attempt = 0u32;
        loop {
            self.status(ConnectionState::Connecting);
            let outcome = self.run_once(&target).await;
            self.close_terminals();
            match outcome {
                Outcome::Shutdown => return,
                Outcome::Permanent(message) => {
                    self.status(ConnectionState::Failed(message));
                    return self.idle_until_dropped().await;
                }
                Outcome::Lost {
                    error,
                    connected_for,
                } => {
                    if connected_for.is_some() {
                        attempt = 0;
                    }
                    attempt += 1;
                    let delay = self.options.backoff.next_delay(connected_for);
                    self.status(ConnectionState::Reconnecting {
                        attempt,
                        retry_in_ms: delay.as_millis() as u64,
                        error,
                    });
                    if !self.wait_offline(delay).await {
                        return;
                    }
                }
            }
        }
    }

    /// Wait out a backoff delay, answering requests as an offline backend.
    /// `false`: the backend was dropped.
    async fn wait_offline(&mut self, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return true,
                op = self.ops.recv() => match op {
                    None => return false,
                    Some(op) => self.offline_op(op),
                },
            }
        }
    }

    async fn idle_until_dropped(mut self) {
        while let Some(op) = self.ops.recv().await {
            self.offline_op(op);
        }
    }

    fn offline_op(&mut self, op: Op) {
        match op {
            Op::Command(command) => self.emit(CoreEvent::CommandRejected {
                command_id: command.command_id,
                reason: format!("{} is not connected", self.env.name),
            }),
            Op::OpenThread(thread_id) => self.stream.open(thread_id),
            Op::TerminalOpen { id, .. } => self.emit(CoreEvent::Terminal(TerminalEvent::Failed {
                id,
                message: format!("{} is not connected", self.env.name),
            })),
            Op::Login(_)
            | Op::InstallAntigravity
            | Op::ImportT3(_)
            | Op::TerminalInput(..)
            | Op::TerminalResize(..)
            | Op::TerminalClose(_) => {}
        }
    }

    fn close_terminals(&mut self) {
        for (id, _) in std::mem::take(&mut self.terminals) {
            self.emit(CoreEvent::Terminal(TerminalEvent::Exited { id }));
        }
    }

    fn acknowledge(&mut self, command_id: CommandId) {
        self.pending.retain(|(_, c)| c.command_id != command_id);
    }

    fn deliver(&mut self, msgs: Vec<ServerMsg>) {
        for msg in msgs {
            if let ServerMsg::CommandRejected { command_id, .. }
            | ServerMsg::CommandDuplicate { command_id } = &msg
            {
                self.acknowledge(*command_id);
            }
            for event in self.stream.apply(msg) {
                if let CoreEvent::Event(e) = &event
                    && let Some(id) = e.command_id
                {
                    self.acknowledge(id);
                }
                self.emit(event);
            }
        }
    }

    async fn run_once(&mut self, target: &Target) -> Outcome {
        let lost = |error: String| Outcome::Lost {
            error,
            connected_for: None,
        };
        let mut link = match open(target).await {
            Ok(link) => link,
            Err(e) => return lost(e.to_string()),
        };
        let hello = Hello::new(self.options.client_name.clone(), self.stream.resume());
        let auth = match (&self.env.credential, link.local_auth) {
            (_, true) => ClientAuth::Local,
            (Some(cred), false) => ClientAuth::Credential(cred),
            (None, false) => {
                return Outcome::Permanent(format!(
                    "{} is not paired: add it again with a pairing code",
                    self.env.name
                ));
            }
        };
        let session = match handshake(&mut link.reader, &mut link.writer, hello, auth).await {
            Ok(s) => s,
            Err(e @ HandshakeError::Refused { .. }) | Err(e @ HandshakeError::WrongServer)
                if e.is_permanent() =>
            {
                return Outcome::Permanent(e.to_string());
            }
            Err(e) => return lost(e.to_string()),
        };
        let connected_at = Instant::now();
        let lost_after = |error: String| Outcome::Lost {
            error,
            connected_for: Some(connected_at.elapsed()),
        };
        self.stream
            .on_welcome(session.challenge.epoch, session.welcome.resumed);
        self.status(ConnectionState::Connected {
            resumed: session.welcome.resumed,
        });
        self.deliver(session.rest);
        if self.stream.revoked {
            return revoked(&self.env.name);
        }
        let (mut reader, mut writer) = (link.reader, link.writer);
        if self.stream.needs_subscribe() && !self.stream.desired.is_empty() {
            self.stream.ready.clear();
            let threads = self.stream.desired.clone();
            if let Err(e) = send(&mut writer, &ClientMsg::Subscribe { threads }).await {
                return lost_after(e.to_string());
            }
        }
        // Re-send what was not answered before the connection dropped.
        self.pending.retain(|(at, _)| at.elapsed() < PENDING_TTL);
        let resend: Vec<CommandEnvelope> = self.pending.iter().map(|(_, c)| c.clone()).collect();
        for command in resend {
            if let Err(e) = send(&mut writer, &ClientMsg::Command(command)).await {
                return lost_after(e.to_string());
            }
        }
        let mut last_heard = Instant::now();
        let mut last_sent = Instant::now();
        let mut last_ping = Instant::now();
        loop {
            // One timer: the next ping or the silence limit, whichever is first.
            let ping_at = last_ping.max(last_sent.min(last_heard)) + PING_EVERY;
            let wake_at = ping_at.min(last_heard + SILENCE_LIMIT);
            tokio::select! {
                frame = recv(&mut reader) => match frame {
                    Ok(msgs) => {
                        last_heard = Instant::now();
                        self.deliver(msgs);
                        if self.stream.revoked {
                            writer.close().await;
                            return revoked(&self.env.name);
                        }
                    }
                    Err(e) => return lost_after(e.to_string()),
                },
                op = self.ops.recv() => {
                    let Some(op) = op else {
                        writer.close().await;
                        return Outcome::Shutdown;
                    };
                    let msg = match op {
                        Op::Command(command) => {
                            if self.pending.len() >= MAX_PENDING {
                                self.pending.pop_front();
                            }
                            self.pending.push_back((Instant::now(), command.clone()));
                            ClientMsg::Command(command)
                        }
                        Op::OpenThread(thread_id) => {
                            self.stream.open(thread_id);
                            ClientMsg::Subscribe { threads: vec![thread_id] }
                        }
                        Op::Login(provider) => ClientMsg::Login { provider },
                        Op::InstallAntigravity => ClientMsg::InstallAntigravity,
                        Op::ImportT3(path) => ClientMsg::ImportT3 { path },
                        Op::TerminalOpen { id, thread_id, columns, lines } => {
                            self.terminals.insert(id, thread_id);
                            ClientMsg::TerminalOpen { id, thread_id, columns, lines }
                        }
                        Op::TerminalInput(id, data) => ClientMsg::TerminalInput { id, data },
                        Op::TerminalResize(id, columns, lines) => {
                            ClientMsg::TerminalResize { id, columns, lines }
                        }
                        Op::TerminalClose(id) => {
                            self.terminals.remove(&id);
                            ClientMsg::TerminalClose { id }
                        }
                    };
                    if let Err(e) = send(&mut writer, &msg).await {
                        return lost_after(e.to_string());
                    }
                    last_sent = Instant::now();
                }
                _ = tokio::time::sleep_until(wake_at) => {
                    if last_heard.elapsed() >= SILENCE_LIMIT {
                        return lost_after("the server stopped answering".into());
                    }
                    if Instant::now() >= ping_at {
                        let at = crate::secret::unix_now();
                        if let Err(e) = send(&mut writer, &ClientMsg::Ping { at }).await {
                            return lost_after(e.to_string());
                        }
                        last_sent = Instant::now();
                        last_ping = last_sent;
                    }
                }
            }
        }
    }
}

fn revoked(name: &str) -> Outcome {
    Outcome::Permanent(format!(
        "{name} revoked this device; pair it again with a new code"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blongo_protocol::wire::{Sequenced, WireEvent, WireThread};
    use blongo_protocol::{
        DomainEvent, EventKind, ItemId, ItemKind, RunId, RunStatus, ShellSnapshot, Timestamp,
        TurnItem,
    };

    fn delta(seq: u64, thread_id: ThreadId, text: &str) -> ServerMsg {
        ServerMsg::Seq(Sequenced {
            seq,
            payload: Payload::TextDelta {
                thread_id,
                item_id: ItemId(uuid_zero()),
                chunk: text.into(),
            },
        })
    }

    fn uuid_zero() -> blongo_protocol::Uuid {
        blongo_protocol::Uuid::nil()
    }

    fn thread_snapshot(seq: u64, thread_id: ThreadId) -> ServerMsg {
        ServerMsg::Seq(Sequenced {
            seq,
            payload: Payload::Thread(WireThread {
                thread_id,
                sequence: 0,
                runs: vec![],
                items: vec![],
            }),
        })
    }

    fn status_event(seq: u64, thread_id: ThreadId) -> ServerMsg {
        ServerMsg::Seq(Sequenced {
            seq,
            payload: Payload::Event(WireEvent::new(Arc::new(DomainEvent {
                sequence: seq,
                at: Timestamp(0),
                command_id: None,
                kind: EventKind::RunStatusChanged {
                    thread_id,
                    run_id: RunId::new(),
                    status: RunStatus::Running,
                    error: None,
                },
            }))),
        })
    }

    fn text_of(events: &[CoreEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                CoreEvent::TextDelta { chunk, .. } => Some(chunk.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replays_drop_duplicates_and_keep_order() {
        let t = ThreadId::new();
        let mut s = StreamState::default();
        s.on_welcome(7, false);
        s.open(t);
        let mut out = s.apply(ServerMsg::Seq(Sequenced {
            seq: 3,
            payload: Payload::Shell(ShellSnapshot::default()),
        }));
        out.extend(s.apply(thread_snapshot(5, t)));
        for (seq, text) in [(6, "a"), (7, "b"), (8, "c")] {
            out.extend(s.apply(delta(seq, t, text)));
        }
        assert_eq!(text_of(&out), "abc");
        // The connection drops; the client resumes from 8.
        let resume = s.resume().unwrap();
        assert_eq!((resume.epoch, resume.last_seq), (7, 8));
        assert_eq!(resume.threads, vec![t]);
        s.on_welcome(7, true);
        assert!(!s.needs_subscribe());
        // A server replaying from an older point: 7 and 8 are dropped.
        let mut out = Vec::new();
        for (seq, text) in [(7, "b"), (8, "c"), (9, "d"), (10, "e")] {
            out.extend(s.apply(delta(seq, t, text)));
        }
        assert_eq!(text_of(&out), "de");
        assert_eq!(s.duplicates, 2);
    }

    #[test]
    fn no_resume_before_this_epochs_shell() {
        let t = ThreadId::new();
        let mut s = StreamState::default();
        // Welcome arrives, then the link drops before the sidebar snapshot.
        s.on_welcome(4, false);
        s.open(t);
        assert!(s.resume().is_none(), "nothing to resume without the shell");
        // Next connection: a fresh start, so the server sends the shell.
        s.on_welcome(4, false);
        s.apply(ServerMsg::Seq(Sequenced {
            seq: 2,
            payload: Payload::Shell(ShellSnapshot::default()),
        }));
        s.apply(thread_snapshot(3, t));
        let r = s.resume().unwrap();
        assert_eq!((r.epoch, r.last_seq), (4, 3));
        // A resnapshot (overflow) means a new shell is coming: until it is
        // applied the position is not resumable either.
        s.apply(ServerMsg::Resnapshot);
        assert!(s.resume().is_none());
        s.apply(ServerMsg::Seq(Sequenced {
            seq: 9,
            payload: Payload::Shell(ShellSnapshot::default()),
        }));
        assert_eq!(s.resume().unwrap().last_seq, 9);
        // A resumed welcome keeps it.
        s.on_welcome(4, true);
        assert!(s.resume().is_some());
    }

    #[test]
    fn a_fresh_start_forgets_held_timelines() {
        let t = ThreadId::new();
        let mut s = StreamState::default();
        s.on_welcome(1, false);
        s.open(t);
        s.apply(thread_snapshot(4, t));
        s.apply(delta(5, t, "x"));
        // The server restarted (new epoch, no resume).
        s.on_welcome(2, false);
        assert_eq!(s.last_seq, 0);
        assert!(s.ready.is_empty());
        assert!(s.needs_subscribe());
        // Deltas for a timeline not held yet are ignored until its snapshot.
        assert!(s.apply(delta(1, t, "lost")).is_empty());
        assert_eq!(s.apply(thread_snapshot(2, t)).len(), 1);
        assert_eq!(text_of(&s.apply(delta(3, t, "y"))), "y");
    }

    #[test]
    fn other_threads_and_resnapshots() {
        let (a, b) = (ThreadId::new(), ThreadId::new());
        let mut s = StreamState::default();
        s.on_welcome(1, false);
        s.open(a);
        s.apply(thread_snapshot(1, a));
        // Sidebar-level events of any thread pass; timeline events of a
        // thread not open do not.
        assert_eq!(s.apply(status_event(2, b)).len(), 1);
        assert!(s.apply(delta(3, b, "no")).is_empty());
        // A snapshot of a thread no longer wanted is ignored.
        assert!(s.apply(thread_snapshot(4, b)).is_empty());
        // Resnapshot: nothing is held until the new snapshots arrive.
        assert!(s.apply(ServerMsg::Resnapshot).is_empty());
        assert!(s.apply(delta(5, a, "no")).is_empty());
        assert_eq!(s.apply(thread_snapshot(9, a)).len(), 1);
        assert_eq!(text_of(&s.apply(delta(10, a, "ok"))), "ok");
    }

    #[test]
    fn item_bodies_survive_the_wire() {
        let t = ThreadId::new();
        let mut s = StreamState::default();
        s.on_welcome(1, false);
        s.open(t);
        s.apply(thread_snapshot(1, t));
        let item = TurnItem {
            id: ItemId::new(),
            thread_id: t,
            run_id: None,
            ordinal: 0,
            created_at: Timestamp(0),
            kind: ItemKind::UserMessage,
            text: "hello".into(),
        };
        let out = s.apply(ServerMsg::Seq(Sequenced {
            seq: 2,
            payload: Payload::Event(WireEvent::new(Arc::new(DomainEvent {
                sequence: 1,
                at: Timestamp(0),
                command_id: None,
                kind: EventKind::ItemAdded {
                    item: Arc::new(item),
                },
            }))),
        }));
        let [CoreEvent::Event(e)] = out.as_slice() else {
            panic!()
        };
        let EventKind::ItemAdded { item } = &e.kind else {
            panic!()
        };
        assert_eq!(&*item.text, "hello");
    }
}
