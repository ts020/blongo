//! Server-side terminals: a shell on a PTY in the thread's working folder,
//! its output streamed to the connection that opened it.
//!
//! Output never gets dropped: when the connection's outbox is half full the
//! reader thread waits, which back-pressures the shell through the PTY.
//! Terminals belong to their connection and end with it (the client shows
//! "[process exited]"). Closing hangs up the shell this server started —
//! only that process — and reaps it off the async runtime.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use blongo_protocol::wire::ServerMsg;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::outbox::Outbox;

/// Terminals one connection may have open.
pub const MAX_TERMINALS: usize = 4;
const CHUNK: usize = 32 * 1024;

struct Terminal {
    writer: mpsc::Sender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    closed: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct Terminals {
    open: HashMap<u32, Terminal>,
}

fn size(columns: u16, lines: u16) -> PtySize {
    PtySize {
        rows: lines.clamp(2, 500),
        cols: columns.clamp(10, 1000),
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Terminals {
    pub fn open(
        &mut self,
        id: u32,
        cwd: &Path,
        columns: u16,
        lines: u16,
        outbox: Arc<Outbox>,
    ) -> anyhow::Result<()> {
        if self.open.contains_key(&id) {
            anyhow::bail!("terminal {id} is already open");
        }
        if self.open.len() >= MAX_TERMINALS {
            anyhow::bail!("at most {MAX_TERMINALS} terminals per connection");
        }
        let pty = native_pty_system().openpty(size(columns, lines))?;
        let shell = std::env::var("BLONGO_TERMINAL_SHELL")
            .ok()
            .or_else(|| std::env::var("SHELL").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pty.slave.spawn_command(cmd)?;
        drop(pty.slave);
        // Our own duplicate of the master, polled with a timeout so the
        // reader stops on close even if a background job keeps the PTY open.
        let fd = pty
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow::anyhow!("the PTY has no file descriptor"))?;
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut reader = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(dup) });
        let mut pty_writer = pty.master.take_writer()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("blongo-serve-pty-w".into())
            .spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    if pty_writer
                        .write_all(&bytes)
                        .and_then(|_| pty_writer.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            })?;
        let closed = Arc::new(AtomicBool::new(false));
        let stop = closed.clone();
        std::thread::Builder::new()
            .name("blongo-serve-pty-r".into())
            .spawn(move || {
                let mut buf = vec![0u8; CHUNK];
                loop {
                    if stop.load(Ordering::Relaxed) || !outbox.wait_room_blocking() {
                        return;
                    }
                    if !readable(&reader) {
                        continue;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            outbox.push(ServerMsg::TerminalOutput {
                                id,
                                data: buf[..n].to_vec(),
                            });
                        }
                    }
                }
                if !stop.load(Ordering::Relaxed) {
                    outbox.push(ServerMsg::TerminalExited { id });
                }
            })?;
        self.open.insert(
            id,
            Terminal {
                writer: tx,
                master: pty.master,
                child: Some(child),
                closed,
            },
        );
        Ok(())
    }

    pub fn input(&mut self, id: u32, data: Vec<u8>) {
        if let Some(t) = self.open.get(&id) {
            let _ = t.writer.send(data);
        }
    }

    pub fn resize(&mut self, id: u32, columns: u16, lines: u16) {
        if let Some(t) = self.open.get(&id) {
            let _ = t.master.resize(size(columns, lines));
        }
    }

    pub fn close(&mut self, id: u32) {
        if let Some(t) = self.open.remove(&id) {
            hang_up(t);
        }
    }

    pub fn close_all(&mut self) {
        for (_, t) in self.open.drain() {
            hang_up(t);
        }
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn hang_up(mut t: Terminal) {
    t.closed.store(true, Ordering::Relaxed);
    let Some(mut child) = t.child.take() else {
        return;
    };
    let pid = child.process_id();
    std::thread::spawn(move || {
        if let Some(pid) = pid {
            if matches!(child.try_wait(), Ok(None)) {
                unsafe { libc::kill(pid as i32, libc::SIGHUP) };
            }
            for _ in 0..20 {
                if !matches!(child.try_wait(), Ok(None)) {
                    drop(t);
                    return;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if matches!(child.try_wait(), Ok(None)) {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
        let _ = child.wait();
        drop(t);
    });
}

/// Wait up to 100 ms for the PTY to have output (or hang up).
fn readable(file: &std::fs::File) -> bool {
    let mut fds = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut fds, 1, 100) > 0 }
}
