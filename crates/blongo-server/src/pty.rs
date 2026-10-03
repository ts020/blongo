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
    writer: mpsc::SyncSender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    child: Option<Box<dyn Child + Send + Sync>>,
    closed: Arc<AtomicBool>,
    /// Write end of the reader's wake pipe: closing it stops the reader at
    /// once (it blocks in `poll` without a timeout).
    wake: Option<OwnedFd>,
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
        // Our own duplicate of the master, polled together with a wake pipe
        // so the reader stops on close even if a background job keeps the
        // PTY open, without waking up periodically.
        let fd = pty
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow::anyhow!("the PTY has no file descriptor"))?;
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut reader = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(dup) });
        let (wake_read, wake_write) = pipe()?;
        let mut pty_writer = pty.master.take_writer()?;
        // Bounded: a shell that stops reading its input cannot make the
        // server buffer a client's keystrokes without limit.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(crate::conn::TERMINAL_INPUT_QUEUE);
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
                    if !readable(&reader, &wake_read) {
                        break;
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
                wake: Some(wake_write),
            },
        );
        Ok(())
    }

    /// Queue input for the shell. When the shell has not read the last
    /// [`crate::conn::TERMINAL_INPUT_QUEUE`] chunks, this one is dropped
    /// (`false`; the client is told). Unknown terminals ignore input.
    pub fn input(&mut self, id: u32, data: Vec<u8>) -> bool {
        match self.open.get(&id) {
            Some(t) => !matches!(t.writer.try_send(data), Err(mpsc::TrySendError::Full(_))),
            None => true,
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
    drop(t.wake.take());
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

/// A close-on-exec pipe: (read end, write end). `std::io::pipe` sets
/// close-on-exec atomically where the OS can (`pipe2` on Linux) and right
/// after creation elsewhere (macOS has no `pipe2`).
fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = std::io::pipe()?;
    Ok((OwnedFd::from(read), OwnedFd::from(write)))
}

/// Block until the PTY has output or hung up (`true`: read it), or the
/// terminal was closed (`false`: the wake pipe's write end is gone).
fn readable(file: &std::fs::File, wake: &OwnedFd) -> bool {
    let mut fds = [
        libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if fds[1].revents != 0 {
            return false;
        }
        return fds[0].revents != 0;
    }
}
