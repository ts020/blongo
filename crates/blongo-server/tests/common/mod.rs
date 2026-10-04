//! Shared helpers: a `blongo serve` over the fake Codex in a scratch
//! directory, paired clients, a severable TCP proxy.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use blongo_client::Backend;
pub use blongo_client::environments::Environment;
pub use blongo_client::remote::{RemoteBackend, RemoteOptions, connect_on};
pub use blongo_core::CoreConfig;
pub use blongo_protocol::client::{ConnectionState, CoreEvent};
pub use blongo_protocol::{
    ApprovalDecision, ApprovalState, Command, CommandEnvelope, Delivery, EventKind, ItemId,
    ItemKind, ProjectId, RunId, RunStatus, ThreadId,
};
pub use blongo_server::{ServeConfig, ServerHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
pub use tokio::sync::mpsc::UnboundedReceiver;

pub fn fixture(name: &str) -> PathBuf {
    runnable(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
}

pub fn fake_codex() -> PathBuf {
    runnable(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../blongo-harness/tests/fixtures/fake_codex.py"),
    )
}

/// The fake agents are Python scripts that run through their `#!` line.
/// Windows has no shebangs, so there they run through a `.cmd` wrapper
/// that calls `python` in UTF-8 mode (the fixtures are UTF-8; Windows'
/// default code page would garble them).
pub fn runnable(script: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let dir = std::env::temp_dir().join(format!("blongo-test-shims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join(script.file_stem().unwrap()).with_extension("cmd");
        if !shim.exists() {
            // Tests run in parallel: write aside, then move into place.
            let tmp = dir.join(format!(
                "{}.{:?}.tmp",
                script.file_stem().unwrap().to_string_lossy(),
                std::thread::current().id()
            ));
            std::fs::write(
                &tmp,
                format!(
                    "@set PYTHONUTF8=1\r\n@python \"{}\" %*\r\n",
                    script.display()
                ),
            )
            .unwrap();
            if std::fs::rename(&tmp, &shim).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        shim
    }
    #[cfg(not(windows))]
    script
}

/// A scratch directory removed when the test ends.
pub struct TempDir(pub PathBuf);

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn temp_dir(name: &str) -> TempDir {
    // macOS's per-user $TMPDIR (/var/folders/../T/) is ~50 bytes: with it
    // the sockets under the data folder would not fit sun_path (104).
    let base = if cfg!(target_os = "macos") {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("blongo-serve-{name}-{}", ThreadId::new()));
    std::fs::create_dir_all(dir.join("project")).unwrap();
    TempDir(dir)
}

pub fn config(dir: &Path) -> ServeConfig {
    let mut core = CoreConfig::new(dir.join("data/blongo.sqlite"));
    core.codex_executable = Some(fake_codex());
    core.text_flush_interval = Duration::from_millis(30);
    core.checkpoints = false;
    core.agent_env = vec![("FAKE_CODEX_DELAY_MS".into(), "20".into())];
    let mut config = ServeConfig::new(core);
    config.listen = Some("127.0.0.1:0".parse().unwrap());
    config.limits.refuse_delay = Duration::from_millis(10);
    config
}

pub fn start(dir: &Path) -> ServerHandle {
    blongo_server::start(config(dir)).unwrap()
}

pub fn ws_target(addr: SocketAddr) -> String {
    format!("ws://{addr}/ws")
}

pub fn pairing_code(dir: &Path) -> String {
    blongo_server::auth::AuthStore::open(&dir.join("data/server"))
        .unwrap()
        .add_pairing_code(600)
        .unwrap()
}

pub async fn pair(dir: &Path, target: &str) -> Environment {
    let code = pairing_code(dir);
    blongo_client::pairing::pair("test", target, Some(&code), "test device")
        .await
        .unwrap()
}

pub fn fast_options() -> RemoteOptions {
    RemoteOptions {
        client_name: "test".into(),
        backoff: blongo_client::backoff::Backoff::scaled(0.02),
    }
}

/// Wait (up to 20 s) for an event `f` picks; others are skipped.
pub async fn wait_for<T>(
    rx: &mut UnboundedReceiver<CoreEvent>,
    mut f: impl FnMut(&CoreEvent) -> Option<T>,
) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("event channel closed");
        if let Some(out) = f(&event) {
            return out;
        }
    }
}

pub async fn wait_connected(rx: &mut UnboundedReceiver<CoreEvent>) -> bool {
    wait_for(rx, |e| match e {
        CoreEvent::Connection(ConnectionState::Connected { resumed }) => Some(*resumed),
        CoreEvent::Connection(ConnectionState::Failed(m)) => panic!("connection failed: {m}"),
        _ => None,
    })
    .await
}

/// Create a project + thread through `backend` and open the thread.
pub async fn project_and_thread(
    backend: &RemoteBackend,
    rx: &mut UnboundedReceiver<CoreEvent>,
    dir: &Path,
) -> ThreadId {
    let project_id = ProjectId::new();
    backend.dispatch(CommandEnvelope::new(Command::ProjectCreate {
        project_id,
        name: String::new(),
        path: dir.join("project").to_string_lossy().into_owned(),
    }));
    let thread_id = ThreadId::new();
    backend.dispatch(CommandEnvelope::new(Command::ThreadCreate {
        thread_id,
        project_id,
        title: String::new(),
        provider: Default::default(),
        model: None,
        worktree: false,
        parent_thread_id: None,
    }));
    wait_for(rx, |e| match e {
        CoreEvent::Event(ev) => match &ev.kind {
            EventKind::ThreadCreated { thread } if thread.id == thread_id => Some(()),
            _ => None,
        },
        CoreEvent::CommandRejected { reason, .. } => panic!("rejected: {reason}"),
        _ => None,
    })
    .await;
    backend.open_thread(thread_id);
    wait_for(rx, |e| match e {
        CoreEvent::Thread(s) if s.thread_id == thread_id => Some(()),
        _ => None,
    })
    .await;
    thread_id
}

pub fn send(backend: &RemoteBackend, thread_id: ThreadId, text: &str) {
    backend.dispatch(CommandEnvelope::new(Command::MessageDispatch {
        thread_id,
        message_id: ItemId::new(),
        run_id: RunId::new(),
        text: text.into(),
        delivery: Delivery::Queue,
    }));
}

pub async fn run_finished(rx: &mut UnboundedReceiver<CoreEvent>) -> RunStatus {
    wait_for(rx, |e| match e {
        CoreEvent::RunFinished { status, .. } => Some(*status),
        _ => None,
    })
    .await
}

/// How long a cut link keeps swallowing server output before it closes:
/// longer than the fake Codex's 50 ms between "slow" deltas.
const CUT_SWALLOW: Duration = Duration::from_millis(200);

/// A TCP proxy whose connections can be cut (the server keeps running,
/// the client sees a dropped link) and whose upstream can be changed.
pub struct Proxy {
    pub addr: SocketAddr,
    upstream: Arc<std::sync::Mutex<SocketAddr>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    pub refusing: Arc<AtomicBool>,
}

impl Proxy {
    pub async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = Arc::new(std::sync::Mutex::new(upstream));
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let refusing = Arc::new(AtomicBool::new(false));
        let (up, generation2, refusing2) = (upstream.clone(), generation.clone(), refusing.clone());
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    continue;
                };
                if refusing2.load(Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let target = *up.lock().unwrap();
                let my_gen = generation2.load(Ordering::SeqCst);
                let generation = generation2.clone();
                tokio::spawn(async move {
                    let Ok(server) = TcpStream::connect(target).await else {
                        return;
                    };
                    let (mut cr, mut cw) = client.into_split();
                    let (mut sr, mut sw) = server.into_split();
                    let is_cut = || generation.load(Ordering::SeqCst) != my_gen;
                    // Once cut, what the server sends is swallowed instead of
                    // delivered, so events are always in flight when the link
                    // drops (a fast client would otherwise have them all).
                    let downstream = async {
                        let mut buf = vec![0u8; 16 * 1024];
                        loop {
                            let n = match sr.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            if !is_cut() && cw.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    };
                    let cut = async {
                        while !is_cut() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        tokio::time::sleep(CUT_SWALLOW).await;
                    };
                    tokio::select! {
                        _ = tokio::io::copy(&mut cr, &mut sw) => {}
                        _ = downstream => {}
                        _ = cut => {}
                    }
                });
            }
        });
        Self {
            addr,
            upstream,
            generation,
            refusing,
        }
    }

    /// Drop every connection going through the proxy now.
    pub fn cut(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn set_upstream(&self, addr: SocketAddr) {
        *self.upstream.lock().unwrap() = addr;
    }
}
