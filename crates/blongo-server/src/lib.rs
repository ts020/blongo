//! `blongo serve`: the orchestrator without a window, reachable over
//! Blongo's wire protocol.
//!
//! One process runs the same `blongo-core` the app embeds (its own thread),
//! a hub task that fans the core's events out to connections ([`hub`]),
//! and listeners:
//!
//! - WebSocket on TCP (`ws://ADDR/ws`). Loopback by default. This
//!   machine's Tailscale address (`--tailscale`, asked from `tailscale ip`)
//!   is allowed because WireGuard encrypts it; any other address, a
//!   CGNAT-range one given with `--listen` included, needs `--insecure-listen`
//!   (Blongo has no TLS of its own: put it behind an SSH tunnel, Tailscale,
//!   or a TLS proxy).
//! - A Unix socket in the private state directory (`<data>/server/
//!   blongo.sock`, 0600 in a 0700 directory): filesystem permissions
//!   authenticate, so `blongo-serve --stdio` (run over SSH) bridges to it.
//! - stdin/stdout (`--stdio`) when no server runs yet: one connection,
//!   served in process (no socket is bound for it, so a second `--stdio`
//!   gets its own core only if the first has ended — the data dir lock
//!   refuses two at once; run `blongo-serve` as a daemon for several
//!   clients).
//!
//! Requests that carry an `Origin` header (a web page) are refused, and
//! connections that have not authenticated yet are capped in total and
//! per peer address.
//!
//! Network clients authenticate with a paired device credential plus a
//! proof of possession ([`auth`]).

pub mod auth;
mod conn;
pub mod hub;
pub mod outbox;
mod pty;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use blongo_client::secret::random;
use blongo_client::transport::{stream_halves, ws_config, ws_halves};
use blongo_core::{CoreConfig, CoreEvent};
use blongo_protocol::wire::MAX_CLIENT_FRAME;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::auth::AuthStore;
use crate::hub::{Hub, HubMsg, RingLimits};
use crate::outbox::OutboxLimits;

#[derive(Clone, Copy, Debug)]
pub enum Transport {
    WebSocket,
    Unix,
    Stdio,
}

impl Transport {
    /// The operating system authenticated the peer.
    pub fn is_local(self) -> bool {
        matches!(self, Self::Unix | Self::Stdio)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::Unix => "unix-socket",
            Self::Stdio => "stdio",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub outbox: OutboxLimits,
    pub ring: RingLimits,
    /// Authenticated-or-not WebSocket connections.
    pub max_connections: usize,
    /// Unix-socket connections (a separate pool: network clients cannot
    /// starve the local ones).
    pub max_local_connections: usize,
    /// WebSocket connections still in the handshake, in total and per
    /// peer address.
    pub max_pre_auth: usize,
    pub max_pre_auth_per_ip: usize,
    pub handshake_timeout: Duration,
    /// A connection silent this long is closed (clients ping every 20 s).
    pub idle_timeout: Duration,
    /// A frame write that takes longer drops the connection.
    pub write_timeout: Duration,
    /// Pause before answering a failed authentication.
    pub refuse_delay: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            outbox: OutboxLimits {
                max_bytes: 4 << 20,
                max_msgs: 8192,
            },
            ring: RingLimits {
                max_msgs: 4096,
                max_bytes: 2 << 20,
            },
            max_connections: 64,
            max_local_connections: 16,
            max_pre_auth: 16,
            max_pre_auth_per_ip: 4,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(75),
            write_timeout: Duration::from_secs(30),
            refuse_delay: Duration::from_millis(500),
        }
    }
}

/// Counters for tests and the log.
#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicUsize,
    pub resnapshots: AtomicU64,
    pub replayed: AtomicU64,
    pub refused: AtomicU64,
    pub ring_bytes: AtomicUsize,
    /// Largest state backlog any closed connection had queued.
    pub peak_outbox_bytes: AtomicUsize,
}

/// Messages queued for the hub before connection readers wait.
const HUB_QUEUE: usize = 1024;

pub(crate) struct Shared {
    pub hub: mpsc::Sender<HubMsg>,
    pub auth: Arc<AuthStore>,
    pub epoch: u64,
    pub limits: Limits,
    pub stats: Arc<Stats>,
    pub next_conn: AtomicU64,
    pre_auth: Arc<Semaphore>,
    pre_auth_by_ip: Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>>,
}

/// A connection's place among those still authenticating; released when
/// the handshake ends.
pub(crate) struct PreAuthSlot {
    _permit: OwnedSemaphorePermit,
    ip: IpAddr,
    by_ip: Arc<std::sync::Mutex<std::collections::HashMap<IpAddr, usize>>>,
}

impl Drop for PreAuthSlot {
    fn drop(&mut self) {
        let mut map = self.by_ip.lock().expect("pre-auth");
        if let Some(n) = map.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

impl Shared {
    fn pre_auth_slot(&self, ip: IpAddr) -> Option<PreAuthSlot> {
        let permit = self.pre_auth.clone().try_acquire_owned().ok()?;
        let mut map = self.pre_auth_by_ip.lock().expect("pre-auth");
        let n = map.entry(ip).or_insert(0);
        if *n >= self.limits.max_pre_auth_per_ip {
            return None;
        }
        *n += 1;
        Some(PreAuthSlot {
            _permit: permit,
            ip,
            by_ip: self.pre_auth_by_ip.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ServeConfig {
    pub core: CoreConfig,
    /// Private state: identity, devices, pairing codes, the Unix socket.
    pub state_dir: PathBuf,
    /// WebSocket listener (`None`: none).
    pub listen: Option<SocketAddr>,
    /// Allow a non-loopback listen address other than this machine's
    /// Tailscale address.
    pub insecure_listen: bool,
    /// `listen` is this machine's Tailscale address (from `tailscale ip`).
    pub tailscale_listen: bool,
    /// Listen on `<state_dir>/blongo.sock`.
    pub unix_socket: bool,
    pub limits: Limits,
}

impl ServeConfig {
    pub fn new(core: CoreConfig) -> Self {
        let state_dir = core.data_dir.join("server");
        Self {
            core,
            state_dir,
            listen: Some(SocketAddr::from((
                [127, 0, 0, 1],
                blongo_client::target::DEFAULT_PORT,
            ))),
            insecure_listen: false,
            tailscale_listen: false,
            unix_socket: true,
            limits: Limits::default(),
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        socket_path(&self.state_dir)
    }
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("blongo.sock")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressClass {
    Loopback,
    /// Tailscale's CGNAT range or its IPv6 ULA prefix.
    Tailscale,
    Other,
}

pub fn classify(ip: IpAddr) -> AddressClass {
    if ip.is_loopback() {
        return AddressClass::Loopback;
    }
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            // 100.64.0.0/10
            if o[0] == 100 && (o[1] & 0xc0) == 64 {
                return AddressClass::Tailscale;
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            // fd7a:115c:a1e0::/48
            if s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0 {
                return AddressClass::Tailscale;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify(IpAddr::V4(v4));
            }
        }
    }
    AddressClass::Other
}

/// Refuse to listen where traffic would cross a network unencrypted,
/// unless explicitly allowed. A Tailscale-range address counts as
/// encrypted only when it is this machine's tailnet address (`tailscale`:
/// it came from `tailscale ip`); a CGNAT address given by hand may be any
/// carrier network.
pub fn check_listen(
    addr: SocketAddr,
    insecure: bool,
    tailscale: bool,
) -> Result<AddressClass, String> {
    let class = classify(addr.ip());
    let protected = match class {
        AddressClass::Loopback => true,
        AddressClass::Tailscale => tailscale,
        AddressClass::Other => false,
    };
    if !protected && !insecure {
        return Err(format!(
            "refusing to listen on {addr}: Blongo has no TLS of its own, so prompts, code and \
             tokens would cross the network unencrypted. Listen on loopback and use an SSH \
             tunnel, listen on this machine's Tailscale address (--tailscale), or pass \
             --insecure-listen if a TLS proxy or a private network protects it"
        ));
    }
    Ok(class)
}

/// This machine's Tailscale IPv4 address (`tailscale ip -4`, or
/// `$BLONGO_TAILSCALE ip -4`).
pub fn tailscale_ip() -> Result<IpAddr, String> {
    let program = std::env::var("BLONGO_TAILSCALE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tailscale".into());
    let out = std::process::Command::new(&program)
        .args(["ip", "-4"])
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{program} ip -4` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let ip: IpAddr = text
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .map_err(|_| format!("`{program} ip -4` printed no address"))?;
    if classify(ip) != AddressClass::Tailscale {
        return Err(format!("{ip} is not a Tailscale address"));
    }
    Ok(ip)
}

/// A running server (on its own threads).
pub struct ServerHandle {
    pub addr: Option<SocketAddr>,
    pub socket: Option<PathBuf>,
    pub server_id: String,
    pub stats: Arc<Stats>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServerHandle {
    /// Stop listening, drop connections and shut the core down cleanly.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Start a server on a new thread; returns once it listens (or failed to).
pub fn start(config: ServeConfig) -> anyhow::Result<ServerHandle> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("blongo-serve".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let stop = async {
                    let _ = stop_rx.await;
                };
                run(config, None, stop, ready_tx).await;
            });
        })?;
    match ready_rx.recv() {
        Ok(Ok(ready)) => Ok(ServerHandle {
            addr: ready.addr,
            socket: ready.socket,
            server_id: ready.server_id,
            stats: ready.stats,
            stop: Some(stop_tx),
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(anyhow::anyhow!(e))
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow::anyhow!("the server thread exited"))
        }
    }
}

pub struct Ready {
    pub addr: Option<SocketAddr>,
    pub socket: Option<PathBuf>,
    pub server_id: String,
    pub stats: Arc<Stats>,
}

/// Serve until `stop` resolves (or, with `stdio`, until that one
/// connection ends). `ready` hears once the listeners are up.
pub async fn run(
    config: ServeConfig,
    stdio: Option<(
        blongo_client::transport::Reader,
        blongo_client::transport::Writer,
    )>,
    stop: impl std::future::Future<Output = ()>,
    ready: std::sync::mpsc::Sender<Result<Ready, String>>,
) {
    let fail = |e: String| {
        let _ = ready.send(Err(e));
    };
    if let Some(addr) = config.listen
        && let Err(e) = check_listen(addr, config.insecure_listen, config.tailscale_listen)
    {
        return fail(e);
    }
    let auth = match AuthStore::open(&config.state_dir) {
        Ok(a) => Arc::new(a),
        Err(e) => return fail(format!("cannot open {}: {e}", config.state_dir.display())),
    };
    let (core, mut core_events) = match blongo_core::spawn(config.core.clone()) {
        Ok(c) => c,
        Err(e) => return fail(format!("cannot start the core: {e:#}")),
    };
    // The core's first word: its shell snapshot, or why it cannot start.
    match core_events.recv().await {
        Some(CoreEvent::Shell(_)) => {}
        Some(CoreEvent::Failed { message }) => {
            core.shutdown();
            return fail(message);
        }
        other => {
            core.shutdown();
            return fail(format!("unexpected first core event: {other:?}"));
        }
    }
    let tcp = match config.listen {
        Some(addr) => match TcpListener::bind(addr).await {
            Ok(l) => Some(l),
            Err(e) => {
                core.shutdown();
                return fail(format!("cannot listen on {addr}: {e}"));
            }
        },
        None => None,
    };
    let socket = config.socket_path();
    let unix = if config.unix_socket {
        match bind_unix(&socket).await {
            Ok(l) => Some(l),
            Err(e) => {
                core.shutdown();
                return fail(e);
            }
        }
    } else {
        None
    };
    let stats = Arc::new(Stats::default());
    let epoch = u64::from_be_bytes(random::<8>());
    let hub = Hub::new(core.client(), epoch, config.limits.ring, stats.clone());
    let (hub_tx, hub_rx) = mpsc::channel(HUB_QUEUE);
    let hub_task = tokio::spawn(hub.run(core_events, hub_rx));
    let shared = Arc::new(Shared {
        hub: hub_tx,
        auth: auth.clone(),
        epoch,
        limits: config.limits.clone(),
        stats: stats.clone(),
        next_conn: AtomicU64::new(1),
        pre_auth: Arc::new(Semaphore::new(config.limits.max_pre_auth)),
        pre_auth_by_ip: Arc::default(),
    });
    let addr = tcp.as_ref().and_then(|l| l.local_addr().ok());
    let _ = ready.send(Ok(Ready {
        addr,
        socket: unix.as_ref().map(|_| socket.clone()),
        server_id: auth.server_id().to_owned(),
        stats: stats.clone(),
    }));
    let slots = Arc::new(Semaphore::new(config.limits.max_connections));
    let local_slots = Arc::new(Semaphore::new(config.limits.max_local_connections));
    let tcp_task = tcp.map(|l| tokio::spawn(accept_tcp(l, shared.clone(), slots)));
    let unix_task = unix.map(|l| tokio::spawn(accept_unix(l, shared.clone(), local_slots)));
    match stdio {
        Some((reader, writer)) => {
            tokio::select! {
                _ = conn::serve(reader, writer, Transport::Stdio, "stdio".into(), None, shared.clone()) => {}
                _ = stop => {}
            }
        }
        None => stop.await,
    }
    for t in [tcp_task, unix_task].into_iter().flatten() {
        t.abort();
    }
    if config.unix_socket {
        let _ = std::fs::remove_file(&socket);
    }
    drop(shared);
    // Stop the core (finishes running turns as interrupted), then the hub
    // drains and closes every connection.
    tokio::task::spawn_blocking(move || core.shutdown())
        .await
        .ok();
    let _ = tokio::time::timeout(Duration::from_secs(5), hub_task).await;
}

async fn bind_unix(path: &Path) -> Result<UnixListener, String> {
    if let Some(dir) = path.parent() {
        blongo_client::secret::private_dir(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if path.exists() {
        // A live server answers; a stale socket file does not.
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Err(format!(
                "another blongo-serve is already running ({})",
                path.display()
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    // sun_path holds 108 bytes including the terminating NUL.
    if path.as_os_str().len() > 107 {
        return Err(format!(
            "the socket path {} is too long for a Unix socket; use a shorter --data-dir \
             or --no-socket",
            path.display()
        ));
    }
    // The folder is private (0700), so nobody else can reach the socket
    // even before the chmod below. No process-wide umask change: it would
    // also apply to files other threads create meanwhile.
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("cannot listen on {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

/// Accept the WebSocket upgrade only on `/ws`.
#[allow(clippy::result_large_err)] // the callback type is tungstenite's
fn only_ws_path(
    req: &tokio_tungstenite::tungstenite::handshake::server::Request,
    resp: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    // Browsers always send Origin; Blongo clients never do. Refusing it
    // keeps web pages (which can open WebSockets to localhost) out.
    if req.headers().contains_key("origin") {
        let mut forbidden =
            tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(None);
        *forbidden.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN;
        return Err(forbidden);
    }
    if req.uri().path() == "/ws" {
        Ok(resp)
    } else {
        let mut not_found =
            tokio_tungstenite::tungstenite::handshake::server::ErrorResponse::new(None);
        *not_found.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
        Err(not_found)
    }
}

async fn accept_tcp(listener: TcpListener, shared: Arc<Shared>, slots: Arc<Semaphore>) {
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            eprintln!("blongo-serve: too many connections; refusing {peer}");
            continue;
        };
        let Some(pre_auth) = shared.pre_auth_slot(peer.ip()) else {
            eprintln!("blongo-serve: too many unauthenticated connections; refusing {peer}");
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = stream.set_nodelay(true);
            let accept = tokio_tungstenite::accept_hdr_async_with_config(
                stream,
                only_ws_path,
                Some(ws_config(MAX_CLIENT_FRAME)),
            );
            let ws = match tokio::time::timeout(shared.limits.handshake_timeout, accept).await {
                Ok(Ok(ws)) => ws,
                _ => return,
            };
            let (reader, writer) = ws_halves(ws);
            conn::serve(
                reader,
                writer,
                Transport::WebSocket,
                peer.to_string(),
                Some(pre_auth),
                shared,
            )
            .await;
        });
    }
}

async fn accept_unix(listener: UnixListener, shared: Arc<Shared>, slots: Arc<Semaphore>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let (read, write) = stream.into_split();
            let (reader, writer) = stream_halves(read, write, MAX_CLIENT_FRAME);
            conn::serve(reader, writer, Transport::Unix, "unix".into(), None, shared).await;
        });
    }
}

/// `--stdio` when a server already runs: copy bytes between stdin/stdout
/// and its Unix socket (frames pass through untouched). `Ok(false)`: no
/// server is running.
pub async fn bridge_stdio(state_dir: &Path) -> std::io::Result<bool> {
    let path = socket_path(state_dir);
    let Ok(stream) = tokio::net::UnixStream::connect(&path).await else {
        return Ok(false);
    };
    let (mut sock_r, mut sock_w) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = tokio::io::copy(&mut stdin, &mut sock_w);
    let down = tokio::io::copy(&mut sock_r, &mut stdout);
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(true)
}

/// Revoke a device through a running server's local socket, so its live
/// connections (and their terminals) end now. `Ok(None)`: no server runs
/// on this data directory.
pub async fn revoke_via_socket(state_dir: &Path, who: &str) -> Result<Option<usize>, String> {
    use blongo_client::handshake::{self, ClientAuth};
    use blongo_protocol::wire::{ClientMsg, Hello, MAX_SERVER_FRAME, ServerMsg};
    let Ok(stream) = tokio::net::UnixStream::connect(socket_path(state_dir)).await else {
        return Ok(None);
    };
    let (read, write) = stream.into_split();
    let (mut reader, mut writer) =
        blongo_client::transport::stream_halves(read, write, MAX_SERVER_FRAME);
    let work = async {
        handshake::handshake(
            &mut reader,
            &mut writer,
            Hello::new("blongo-serve revoke", None),
            ClientAuth::Local,
        )
        .await
        .map_err(|e| e.to_string())?;
        handshake::send(
            &mut writer,
            &ClientMsg::Revoke {
                device: who.to_owned(),
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        loop {
            for msg in handshake::recv(&mut reader)
                .await
                .map_err(|e| e.to_string())?
            {
                if let ServerMsg::Revoked(result) = msg {
                    return result.map(Some);
                }
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(30), work)
        .await
        .map_err(|_| "the server did not answer".to_string())?;
    writer.close().await;
    result
}

/// Approximate counters as text (the log).
pub fn describe(stats: &Stats) -> String {
    format!(
        "{} connections, {} resnapshots, {} replayed, {} refused",
        stats.connections.load(Ordering::Relaxed),
        stats.resnapshots.load(Ordering::Relaxed),
        stats.replayed.load(Ordering::Relaxed),
        stats.refused.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_policy() {
        let ok = |s: &str| check_listen(s.parse().unwrap(), false, true);
        assert_eq!(ok("127.0.0.1:1").unwrap(), AddressClass::Loopback);
        assert_eq!(ok("[::1]:1").unwrap(), AddressClass::Loopback);
        assert_eq!(ok("100.101.102.103:1").unwrap(), AddressClass::Tailscale);
        assert_eq!(ok("100.64.0.1:1").unwrap(), AddressClass::Tailscale);
        assert_eq!(
            ok("[fd7a:115c:a1e0::1]:1").unwrap(),
            AddressClass::Tailscale
        );
        // The CGNAT range given by hand (not from `tailscale ip`): refused.
        assert!(check_listen("100.101.102.103:1".parse().unwrap(), false, false).is_err());
        assert!(check_listen("127.0.0.1:1".parse().unwrap(), false, false).is_ok());
        assert!(ok("100.128.0.1:1").is_err());
        assert!(ok("0.0.0.0:1").is_err());
        assert!(ok("192.168.1.2:1").is_err());
        assert!(ok("[::]:1").is_err());
        assert_eq!(
            check_listen("0.0.0.0:1".parse().unwrap(), true, false).unwrap(),
            AddressClass::Other
        );
    }
}
