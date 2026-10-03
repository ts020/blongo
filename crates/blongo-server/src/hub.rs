//! The hub: the one task that reads the core's event channel and fans it
//! out to connections.
//!
//! - Every state message gets the next sequence number of this server
//!   process (`epoch`) and is kept in a bounded replay ring.
//! - A connection holds the sidebar (after its shell snapshot) and the
//!   timelines it subscribed to (after each thread's snapshot). Sidebar
//!   events go to every connection holding the sidebar; item events only to
//!   connections holding that thread. Snapshots come from the core itself,
//!   in its channel order, so everything after a snapshot is newer than it.
//! - A reconnecting client sends its `(epoch, last_seq)`: when the ring
//!   still has everything after `last_seq`, the hub replays exactly that;
//!   otherwise it sends fresh snapshots.
//! - A connection whose outbox overflows (slow reader) loses its state
//!   backlog, gets `Resnapshot` and fresh snapshots; one that keeps
//!   overflowing is disconnected.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use blongo_core::{CoreClient, CoreEvent};
use blongo_protocol::wire::{
    IssuedCredential, Payload, Resume, Sequenced, ServerMsg, Welcome, WireEvent, WireThread,
};
use blongo_protocol::{CommandId, EventKind, ModelInfo, ProjectId, ProviderKind, ThreadId};
use tokio::sync::{mpsc, oneshot};

use crate::Stats;
use crate::outbox::{Outbox, Push};

pub type ConnId = u64;

/// A connection that overflows more often than this within
/// [`OVERFLOW_WINDOW`] is dropped (it can never catch up).
const MAX_OVERFLOWS: usize = 5;
const OVERFLOW_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_ROUTED_COMMANDS: usize = 4096;
const MAX_SUBSCRIBED_THREADS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct RingLimits {
    pub max_msgs: usize,
    pub max_bytes: usize,
}

pub enum HubMsg {
    Join {
        conn: ConnId,
        outbox: Arc<Outbox>,
        resume: Option<Resume>,
        device_id: Option<String>,
        issued: Option<IssuedCredential>,
    },
    Command(ConnId, blongo_protocol::CommandEnvelope),
    Subscribe(ConnId, Vec<ThreadId>),
    Login(ProviderKind),
    InstallAntigravity,
    ImportT3(Option<String>),
    /// Where a thread works (for server-side terminals).
    Cwd(ThreadId, oneshot::Sender<Option<String>>),
    Leave(ConnId),
}

struct Conn {
    outbox: Arc<Outbox>,
    shell_ready: bool,
    /// Subscribed threads → snapshot delivered.
    threads: HashMap<ThreadId, bool>,
    overflows: VecDeque<std::time::Instant>,
}

impl Conn {
    fn holds(&self, payload: &Payload) -> bool {
        match payload.timeline_thread() {
            None => self.shell_ready,
            Some(t) => self.threads.get(&t) == Some(&true),
        }
    }
}

pub struct Hub {
    core: CoreClient,
    epoch: u64,
    seq: u64,
    ring: VecDeque<(u64, Payload, usize)>,
    ring_bytes: usize,
    ring_limits: RingLimits,
    conns: HashMap<ConnId, Conn>,
    shell_requests: u32,
    pending_shell: Vec<ConnId>,
    pending_threads: HashMap<ThreadId, Vec<ConnId>>,
    /// Who sent a command, to route its rejection / duplicate notice.
    commands: HashMap<CommandId, ConnId>,
    command_order: VecDeque<CommandId>,
    models: HashMap<ProviderKind, Vec<ModelInfo>>,
    /// Working directories for server-side terminals.
    project_paths: HashMap<ProjectId, String>,
    thread_cwd: HashMap<ThreadId, (ProjectId, Option<String>)>,
    failed: Option<String>,
    stats: Arc<Stats>,
}

impl Hub {
    pub fn new(core: CoreClient, epoch: u64, ring_limits: RingLimits, stats: Arc<Stats>) -> Self {
        Self {
            core,
            epoch,
            seq: 0,
            ring: VecDeque::new(),
            ring_bytes: 0,
            ring_limits,
            conns: HashMap::new(),
            shell_requests: 0,
            pending_shell: Vec::new(),
            pending_threads: HashMap::new(),
            commands: HashMap::new(),
            command_order: VecDeque::new(),
            models: HashMap::new(),
            project_paths: HashMap::new(),
            thread_cwd: HashMap::new(),
            failed: None,
            stats,
        }
    }

    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Run until the core's channel closes.
    pub async fn run(
        mut self,
        mut core_events: mpsc::UnboundedReceiver<CoreEvent>,
        mut msgs: mpsc::UnboundedReceiver<HubMsg>,
    ) {
        loop {
            tokio::select! {
                event = core_events.recv() => match event {
                    Some(event) => self.on_core(event),
                    None => break,
                },
                msg = msgs.recv() => match msg {
                    Some(msg) => self.on_msg(msg),
                    None => break,
                },
            }
        }
        for conn in self.conns.values() {
            conn.outbox.push(ServerMsg::Failed {
                message: "the server is shutting down".into(),
            });
            conn.outbox.close();
        }
    }

    // ------------------------------------------------------------ helpers

    fn push(&mut self, conn_id: ConnId, msg: ServerMsg) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        match conn.outbox.push(msg) {
            Push::Queued => {}
            Push::Closed => {}
            Push::Overflow => {
                let now = std::time::Instant::now();
                conn.overflows
                    .retain(|t| now.duration_since(*t) < OVERFLOW_WINDOW);
                conn.overflows.push_back(now);
                self.stats.resnapshots.fetch_add(1, Ordering::Relaxed);
                if conn.overflows.len() > MAX_OVERFLOWS {
                    eprintln!("blongo-serve: connection {conn_id} reads too slowly; closing it");
                    conn.outbox.close();
                    return;
                }
                // Hold nothing until fresh snapshots arrive.
                conn.shell_ready = false;
                let threads: Vec<ThreadId> = conn.threads.keys().copied().collect();
                for ready in conn.threads.values_mut() {
                    *ready = false;
                }
                conn.outbox.push(ServerMsg::Resnapshot);
                self.request_shell(conn_id);
                for t in threads {
                    self.request_thread(conn_id, t);
                }
            }
        }
    }

    fn broadcast(&mut self, msg: ServerMsg) {
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for id in ids {
            self.push(id, msg.clone());
        }
    }

    fn request_shell(&mut self, conn: ConnId) {
        if !self.pending_shell.contains(&conn) {
            self.pending_shell.push(conn);
            self.shell_requests += 1;
            self.core.shell();
        }
    }

    fn request_thread(&mut self, conn: ConnId, thread_id: ThreadId) {
        let waiting = self.pending_threads.entry(thread_id).or_default();
        if !waiting.contains(&conn) {
            waiting.push(conn);
            self.core.open_thread(thread_id);
        }
    }

    /// Give a state payload the next sequence number, keep it for replay
    /// and send it to the connections that hold its part of the state.
    fn sequence(&mut self, payload: Payload) {
        self.seq += 1;
        let seq = self.seq;
        let size = payload.approx_size();
        let targets: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.holds(&payload))
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            self.push(
                id,
                ServerMsg::Seq(Sequenced {
                    seq,
                    payload: payload.clone(),
                }),
            );
        }
        self.ring.push_back((seq, payload, size));
        self.ring_bytes += size;
        while self.ring.len() > self.ring_limits.max_msgs
            || self.ring_bytes > self.ring_limits.max_bytes
        {
            let Some((_, _, s)) = self.ring.pop_front() else {
                break;
            };
            self.ring_bytes -= s;
        }
        self.stats
            .ring_bytes
            .store(self.ring_bytes, Ordering::Relaxed);
    }

    fn route_command_reply(&mut self, command_id: CommandId, msg: ServerMsg) {
        if let Some(conn) = self.commands.remove(&command_id) {
            self.push(conn, msg);
        }
    }

    fn track_cwd(&mut self, kind: &EventKind) {
        match kind {
            EventKind::ProjectCreated { project } => {
                self.project_paths.insert(project.id, project.path.clone());
            }
            EventKind::ThreadCreated { thread } => {
                self.thread_cwd.insert(
                    thread.id,
                    (
                        thread.project_id,
                        thread.worktree.as_ref().map(|w| w.path.clone()),
                    ),
                );
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ core side

    fn on_core(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::Shell(shell) => {
                for p in &shell.projects {
                    self.project_paths.insert(p.id, p.path.clone());
                }
                for t in &shell.threads {
                    self.thread_cwd.insert(
                        t.id,
                        (t.project_id, t.worktree.as_ref().map(|w| w.path.clone())),
                    );
                }
                let targets: Vec<ConnId> = if self.shell_requests > 0 {
                    self.shell_requests -= 1;
                    std::mem::take(&mut self.pending_shell)
                } else {
                    // Unrequested (after an import): everyone's sidebar.
                    self.conns.keys().copied().collect()
                };
                let seq = self.seq;
                for id in targets {
                    if let Some(conn) = self.conns.get_mut(&id) {
                        conn.shell_ready = true;
                    }
                    self.push(
                        id,
                        ServerMsg::Seq(Sequenced {
                            seq,
                            payload: Payload::Shell((*shell).clone()),
                        }),
                    );
                }
            }
            CoreEvent::Thread(snapshot) => {
                let waiting = self
                    .pending_threads
                    .remove(&snapshot.thread_id)
                    .unwrap_or_default();
                if waiting.is_empty() {
                    return;
                }
                let wire = WireThread::from(&*snapshot);
                let seq = self.seq;
                for id in waiting {
                    let Some(conn) = self.conns.get_mut(&id) else {
                        continue;
                    };
                    // Still wanted?
                    let Some(ready) = conn.threads.get_mut(&snapshot.thread_id) else {
                        continue;
                    };
                    *ready = true;
                    self.push(
                        id,
                        ServerMsg::Seq(Sequenced {
                            seq,
                            payload: Payload::Thread(wire.clone()),
                        }),
                    );
                }
            }
            CoreEvent::Event(event) => {
                self.track_cwd(&event.kind);
                if let Some(id) = event.command_id {
                    self.commands.remove(&id);
                }
                self.sequence(Payload::Event(WireEvent::new(event)));
            }
            CoreEvent::TextDelta {
                thread_id,
                item_id,
                chunk,
            } => self.sequence(Payload::TextDelta {
                thread_id,
                item_id,
                chunk,
            }),
            CoreEvent::CommandRejected { command_id, reason } => {
                self.route_command_reply(
                    command_id,
                    ServerMsg::CommandRejected { command_id, reason },
                );
            }
            CoreEvent::CommandDuplicate { command_id } => {
                self.route_command_reply(command_id, ServerMsg::CommandDuplicate { command_id });
            }
            CoreEvent::RunFinished { thread_id, status } => {
                self.broadcast(ServerMsg::RunFinished { thread_id, status })
            }
            CoreEvent::Models { provider, models } => {
                let models = models.to_vec();
                self.models.insert(provider, models.clone());
                self.broadcast(ServerMsg::Models { provider, models });
            }
            CoreEvent::Login { provider, state } => {
                self.broadcast(ServerMsg::Login { provider, state })
            }
            CoreEvent::Install(state) => self.broadcast(ServerMsg::Install(state)),
            CoreEvent::Notice { message } => self.broadcast(ServerMsg::Notice { message }),
            CoreEvent::Imported(result) => self.broadcast(ServerMsg::Imported(result)),
            CoreEvent::Failed { message } => {
                eprintln!("blongo-serve: the core stopped: {message}");
                self.failed = Some(message.clone());
                self.broadcast(ServerMsg::Failed { message });
            }
            CoreEvent::Connection(_) | CoreEvent::Terminal(_) => {}
        }
    }

    // ------------------------------------------------------ connection side

    fn can_resume(&self, resume: &Resume) -> bool {
        if resume.epoch != self.epoch || resume.last_seq > self.seq {
            return false;
        }
        if resume.last_seq == self.seq {
            return true;
        }
        self.ring
            .front()
            .is_some_and(|(first, _, _)| *first <= resume.last_seq + 1)
    }

    fn on_msg(&mut self, msg: HubMsg) {
        match msg {
            HubMsg::Join {
                conn,
                outbox,
                resume,
                device_id,
                issued,
            } => {
                let resumed = resume.as_ref().is_some_and(|r| self.can_resume(r));
                outbox.push(ServerMsg::Welcome(Welcome {
                    device_id,
                    issued,
                    resumed,
                }));
                let mut entry = Conn {
                    outbox,
                    shell_ready: false,
                    threads: HashMap::new(),
                    overflows: VecDeque::new(),
                };
                if let (true, Some(r)) = (resumed, &resume) {
                    entry.shell_ready = true;
                    entry.threads = r
                        .threads
                        .iter()
                        .take(MAX_SUBSCRIBED_THREADS)
                        .map(|t| (*t, true))
                        .collect();
                }
                self.conns.insert(conn, entry);
                self.stats
                    .connections
                    .store(self.conns.len(), Ordering::Relaxed);
                if let (true, Some(r)) = (resumed, &resume) {
                    let replay: Vec<(u64, Payload)> = self
                        .ring
                        .iter()
                        .filter(|(seq, _, _)| *seq > r.last_seq)
                        .filter(|(_, p, _)| self.conns[&conn].holds(p))
                        .map(|(seq, p, _)| (*seq, p.clone()))
                        .collect();
                    self.stats
                        .replayed
                        .fetch_add(replay.len() as u64, Ordering::Relaxed);
                    eprintln!(
                        "blongo-serve: connection {conn}: resumed after seq {}, replaying {}",
                        r.last_seq,
                        replay.len()
                    );
                    for (seq, payload) in replay {
                        self.push(conn, ServerMsg::Seq(Sequenced { seq, payload }));
                    }
                } else {
                    if resume.is_some() {
                        eprintln!(
                            "blongo-serve: connection {conn}: cannot resume (other epoch or too old); sending snapshots"
                        );
                    }
                    self.request_shell(conn);
                }
                let models: Vec<ServerMsg> = self
                    .models
                    .iter()
                    .map(|(provider, models)| ServerMsg::Models {
                        provider: *provider,
                        models: models.clone(),
                    })
                    .collect();
                for m in models {
                    self.push(conn, m);
                }
                if let Some(message) = self.failed.clone() {
                    self.push(conn, ServerMsg::Failed { message });
                }
            }
            HubMsg::Command(conn, command) => {
                if self.commands.len() >= MAX_ROUTED_COMMANDS
                    && let Some(old) = self.command_order.pop_front()
                {
                    self.commands.remove(&old);
                }
                self.commands.insert(command.command_id, conn);
                self.command_order.push_back(command.command_id);
                if self.command_order.len() > 2 * MAX_ROUTED_COMMANDS {
                    let live: HashSet<CommandId> = self.commands.keys().copied().collect();
                    self.command_order.retain(|c| live.contains(c));
                }
                self.core.dispatch(command);
            }
            HubMsg::Subscribe(conn, threads) => {
                let Some(entry) = self.conns.get_mut(&conn) else {
                    return;
                };
                entry.threads = threads
                    .iter()
                    .take(MAX_SUBSCRIBED_THREADS)
                    .map(|t| (*t, false))
                    .collect();
                for t in threads.into_iter().take(MAX_SUBSCRIBED_THREADS) {
                    self.request_thread(conn, t);
                }
            }
            HubMsg::Login(provider) => self.core.login(provider),
            HubMsg::InstallAntigravity => self.core.install_antigravity(),
            HubMsg::ImportT3(path) => {
                let source = path
                    .map(std::path::PathBuf::from)
                    .or_else(blongo_core::t3_import::default_source);
                match source {
                    Some(source) => self.core.import_t3(source),
                    None => self.broadcast(ServerMsg::Imported(Err(
                        "no home directory on the server".into(),
                    ))),
                }
            }
            HubMsg::Cwd(thread_id, reply) => {
                let cwd = self
                    .thread_cwd
                    .get(&thread_id)
                    .and_then(|(p, wt)| wt.clone().or_else(|| self.project_paths.get(p).cloned()));
                let _ = reply.send(cwd);
            }
            HubMsg::Leave(conn) => {
                if let Some(c) = self.conns.remove(&conn) {
                    c.outbox.close();
                    self.stats
                        .peak_outbox_bytes
                        .fetch_max(c.outbox.peak_bytes(), Ordering::Relaxed);
                }
                self.pending_shell.retain(|c| *c != conn);
                for waiting in self.pending_threads.values_mut() {
                    waiting.retain(|c| *c != conn);
                }
                self.stats
                    .connections
                    .store(self.conns.len(), Ordering::Relaxed);
            }
        }
    }
}
