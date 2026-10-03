//! Child-process plumbing shared by every harness: spawn with piped stdio in
//! its own process group, a stdin writer task, a bounded stdout line reader,
//! a stderr drain that keeps only a short tail, and SIGTERM→SIGKILL reaping.
//!
//! Lifecycle adapted from zeron crates/harness/src/lib.rs (`shutdown_child`,
//! `StderrTail`) and crates/harness/src/claude/mod.rs (`stdin_writer`).

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::SessionConfig;

/// Lines longer than this are dropped (with a warning event from the
/// caller) instead of growing a buffer without bound.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// A message for the child's stdin.
#[derive(Debug)]
pub enum StdinMsg {
    Line(String),
    Close,
}

/// A spawned agent with its stdio wired up.
pub struct AgentProcess {
    pub child: Child,
    pub stdin: mpsc::UnboundedSender<StdinMsg>,
    pub stdout: LineReader<ChildStdout>,
    pub stderr: StderrTail,
}

impl AgentProcess {
    /// Queue one line (a newline is appended by the writer).
    pub fn write_line(&self, line: String) {
        let _ = self.stdin.send(StdinMsg::Line(line));
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
}

/// Spawn `program args…` for `config` (cwd, env, extra args) with piped
/// stdio, in a new process group so a shim's grandchildren die with it.
pub fn spawn(
    program: &Path,
    args: &[&OsStr],
    config: &SessionConfig,
    harness_env: &[(&str, &str)],
) -> io::Result<AgentProcess> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .args(&config.extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !config.cwd.as_os_str().is_empty() {
        cmd.current_dir(&config.cwd);
    }
    for key in &config.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in harness_env {
        cmd.env(key, value);
    }
    for (key, value) in &config.env {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    die_with_parent(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("agent executable not found: {}", program.display()),
            )
        } else {
            e
        }
    })?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
    tokio::spawn(stdin_writer(stdin, stdin_rx));
    let tail = StderrTail::default();
    tokio::spawn(drain_stderr(stderr, tail.clone()));
    Ok(AgentProcess {
        child,
        stdin: stdin_tx,
        stdout: LineReader::new(stdout),
        stderr: tail,
    })
}

async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<StdinMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            StdinMsg::Line(mut line) => {
                line.push('\n');
                let write = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.flush().await
                };
                // EPIPE after the child died is expected; the stdout reader
                // reports the exit.
                if write.await.is_err() {
                    return;
                }
            }
            StdinMsg::Close => {
                let _ = stdin.shutdown().await;
                return;
            }
        }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: StderrTail) {
    let mut lines = LineReader::new(stderr);
    while let Ok(Some(line)) = lines.next_line().await {
        tail.push(&line);
    }
}

/// Newline-framed reader with a hard per-line cap. Partial data lives in the
/// struct, so `next_line` is cancel-safe inside `tokio::select!`.
pub struct LineReader<R> {
    inner: BufReader<R>,
    buf: Vec<u8>,
    overflow: bool,
    max: usize,
}

impl<R: tokio::io::AsyncRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_max(inner, MAX_LINE_BYTES)
    }

    pub fn with_max(inner: R, max: usize) -> Self {
        Self {
            // 8 KiB is plenty for JSONL deltas; big frames grow `buf` only
            // for their own lifetime (it is shrunk again below).
            inner: BufReader::with_capacity(8 * 1024, inner),
            buf: Vec::new(),
            overflow: false,
            max,
        }
    }

    /// Next line without the trailing `\n` / `\r\n`; `Ok(None)` at EOF.
    /// Over-long lines are skipped and reported as `Err(InvalidData)`.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                if self.buf.is_empty() && !self.overflow {
                    return Ok(None);
                }
                // EOF without a final newline: flush what we have.
                return self.finish_line().map(Some);
            }
            let (chunk, found) = match available.iter().position(|b| *b == b'\n') {
                Some(at) => (&available[..at], Some(at)),
                None => (available, None),
            };
            if !self.overflow {
                if self.buf.len() + chunk.len() > self.max {
                    self.overflow = true;
                    self.buf = Vec::new();
                } else {
                    self.buf.extend_from_slice(chunk);
                }
            }
            let consumed = found.map_or(available.len(), |at| at + 1);
            self.inner.consume(consumed);
            if found.is_some() {
                return self.finish_line().map(Some);
            }
        }
    }

    fn finish_line(&mut self) -> io::Result<String> {
        if std::mem::take(&mut self.overflow) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("agent wrote a line over {} bytes; skipped", self.max),
            ));
        }
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        let bytes = if self.buf.capacity() > 64 * 1024 {
            // Hand the big allocation to the caller instead of keeping it.
            std::mem::take(&mut self.buf)
        } else {
            let line = self.buf.clone();
            self.buf.clear();
            line
        };
        Ok(String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }
}

/// The last few stderr lines, for crash messages. Bounded.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    const KEEP_LINES: usize = 8;
    const KEEP_LINE_BYTES: usize = 500;

    pub fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self.0.lock().unwrap_or_else(|e| e.into_inner());
        tail.push_back(crate::truncate(line, Self::KEEP_LINE_BYTES));
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    pub fn snapshot(&self) -> String {
        let tail = self.0.lock().unwrap_or_else(|e| e.into_inner());
        tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// "<name> exited (<status>): <stderr tail>".
pub fn crash_message(name: &str, child: &mut Child, stderr: &StderrTail) -> String {
    let status = match child.try_wait() {
        Ok(Some(status)) => status.to_string(),
        _ => "still running".into(),
    };
    let tail = stderr.snapshot();
    if tail.is_empty() {
        format!("{name} exited unexpectedly ({status})")
    } else {
        format!("{name} exited unexpectedly ({status}): {tail}")
    }
}

/// Reap the child: SIGTERM its process group, wait `grace`, then SIGKILL.
pub async fn terminate(child: &mut Child, grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            signal_group(pid, libc::SIGKILL);
        }
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        signal_group(pid, libc::SIGTERM);
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            // Leader gone; make sure no grandchild survives it.
            signal_group(pid, libc::SIGKILL);
            return;
        }
        signal_group(pid, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = grace;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Linux: the agent gets SIGKILL when the thread that spawned it exits, so
/// a crashed or `kill -9`-ed Blongo does not leave Codex running. (The core
/// spawns agents from its own long-lived thread.) Grandchildren in the
/// agent's process group are not covered; the agent normally takes them down
/// itself when its stdin closes.
#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    // SAFETY: getpid is async-signal-safe; the pre_exec closure only calls
    // prctl, getppid and _exit, which are async-signal-safe too.
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died before prctl took effect.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

/// SIGKILL a process group created by [`spawn`] (its leader's pid), for
/// callers that gave up waiting on an orderly shutdown.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    signal_group(pid, libc::SIGKILL);
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: libc::c_int) {
    // SAFETY: kill(2) on the private process group we created at spawn.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

/// Find `name` on `PATH`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Resolve an executable: explicit config, then `env_override`, then PATH.
pub fn resolve_executable(
    configured: Option<&Path>,
    env_override: &str,
    name: &str,
) -> io::Result<PathBuf> {
    if let Some(path) = configured {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os(env_override).filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    find_on_path(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{name} not found on PATH (set {env_override} to override)"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn line_reader_splits_and_caps() {
        let data: &[u8] = b"one\r\ntwo\n0123456789abcdef\nlast";
        let mut reader = LineReader::with_max(data, 10);
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("one"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("two"));
        assert_eq!(
            reader.next_line().await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("last"));
        assert_eq!(reader.next_line().await.unwrap(), None);
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let tail = StderrTail::default();
        for i in 0..20 {
            tail.push(&format!("line {i}"));
        }
        let snap = tail.snapshot();
        assert!(snap.starts_with("line 12"));
        assert!(snap.ends_with("line 19"));
    }
}
