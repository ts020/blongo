//! Where an environment lives and how to reach it.
//!
//! - `ws://HOST:PORT[/path]`: a `blongo serve` listening on loopback, on a
//!   Tailscale address (WireGuard encrypts the link) or behind a proxy.
//!   `wss://` is not built in (no TLS stack in Blongo); see the security
//!   notes in `docs/phase3/report.md`.
//! - `ssh://[USER@]HOST[:SSH_PORT]?port=PORT`: an SSH port forward to a
//!   `blongo serve` that listens on the remote host's loopback.
//! - `ssh+stdio://[USER@]HOST[:SSH_PORT][?command=CMD]`: run `blongo-serve
//!   --stdio` (or `CMD`) over SSH and speak frames on its stdin/stdout. SSH
//!   authenticates the user, so no pairing is needed.
//!
//! The SSH client is `ssh` on PATH or `$BLONGO_SSH`; it runs with
//! `BatchMode=yes` (keys or an agent, never a password prompt).

use std::io;
use std::process::Stdio;
use std::time::Duration;

use blongo_protocol::wire::MAX_SERVER_FRAME;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

use crate::transport::{Reader, Writer, stream_halves, ws_config, ws_halves};

pub const DEFAULT_PORT: u16 = 7878;
pub const DEFAULT_STDIO_COMMAND: &str = "blongo-serve --stdio";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    WebSocket {
        host: String,
        port: u16,
        path: String,
    },
    SshTunnel {
        host: String,
        ssh_port: Option<u16>,
        remote_port: u16,
    },
    SshStdio {
        host: String,
        ssh_port: Option<u16>,
        command: String,
    },
}

impl Target {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("ws://") {
            let (authority, path) = match rest.find('/') {
                Some(i) => (&rest[..i], rest[i..].to_owned()),
                None => (rest, "/ws".to_owned()),
            };
            let (host, port) = split_host_port(authority)?;
            if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("bad host {host:?}"));
            }
            return Ok(Self::WebSocket {
                host,
                port: port.unwrap_or(DEFAULT_PORT),
                path,
            });
        }
        if s.starts_with("wss://") || s.starts_with("https://") {
            return Err(
                "wss:// is not built in: reach the server over an SSH tunnel \
                 (ssh://host?port=N), Tailscale (ws://100.x.y.z:N) or a local TLS proxy"
                    .into(),
            );
        }
        let (stdio, rest) = if let Some(rest) = s.strip_prefix("ssh+stdio://") {
            (true, rest)
        } else if let Some(rest) = s.strip_prefix("ssh://") {
            (false, rest)
        } else {
            return Err(format!(
                "unknown target {s:?} (use ws://, ssh:// or ssh+stdio://)"
            ));
        };
        let (authority, query) = match rest.split_once('?') {
            Some((a, q)) => (a, q),
            None => (rest, ""),
        };
        let authority = authority.trim_end_matches('/');
        let (host, ssh_port) = split_host_port(authority)?;
        check_ssh_destination(&host)?;
        let param = |key: &str| {
            query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == key)
                .map(|(_, v)| percent_decode(v))
        };
        if stdio {
            Ok(Self::SshStdio {
                host,
                ssh_port,
                command: param("command").unwrap_or_else(|| DEFAULT_STDIO_COMMAND.into()),
            })
        } else {
            let remote_port = match param("port") {
                Some(p) => p.parse().map_err(|_| format!("bad port {p:?}"))?,
                None => DEFAULT_PORT,
            };
            Ok(Self::SshTunnel {
                host,
                ssh_port,
                remote_port,
            })
        }
    }

    /// Whether the transport itself authenticates the user (no pairing).
    pub fn is_local_auth(&self) -> bool {
        matches!(self, Self::SshStdio { .. })
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let port = |p: &Option<u16>| p.map(|p| format!(":{p}")).unwrap_or_default();
        match self {
            Self::WebSocket { host, port, path } => write!(f, "ws://{host}:{port}{path}"),
            Self::SshTunnel {
                host,
                ssh_port,
                remote_port,
            } => write!(f, "ssh://{host}{}?port={remote_port}", port(ssh_port)),
            Self::SshStdio {
                host,
                ssh_port,
                command,
            } => {
                write!(f, "ssh+stdio://{host}{}", port(ssh_port))?;
                if command != DEFAULT_STDIO_COMMAND {
                    write!(f, "?command={}", command.replace(' ', "%20"))?;
                }
                Ok(())
            }
        }
    }
}

/// An SSH destination (`[user@]host`) that `ssh` can only read as a
/// destination: no option-looking user or host (`-oProxyCommand=...` would
/// run a local command), no whitespace or control characters. It is also
/// passed after `--`.
fn check_ssh_destination(dest: &str) -> Result<(), String> {
    let (user, host) = match dest.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, dest),
    };
    let bad = |s: &str| {
        s.is_empty() || s.starts_with('-') || s.chars().any(|c| c.is_whitespace() || c.is_control())
    };
    if bad(host) || user.is_some_and(|u| bad(u) || u.contains('@')) {
        return Err(format!("bad SSH destination {dest:?}"));
    }
    Ok(())
}

fn split_host_port(authority: &str) -> Result<(String, Option<u16>), String> {
    if authority.is_empty() {
        return Err("missing host".into());
    }
    // [v6]:port
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("bad host {authority:?}"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| format!("bad port {p:?}"))?),
            None => None,
        };
        return Ok((format!("[{host}]"), port));
    }
    match authority.rsplit_once(':') {
        Some(("", _)) => Err(format!("missing host in {authority:?}")),
        Some((host, port)) if !host.contains(':') => Ok((
            host.to_owned(),
            Some(port.parse().map_err(|_| format!("bad port {port:?}"))?),
        )),
        _ => Ok((authority.to_owned(), None)),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An open transport to a server. Drop it to close; an SSH child it
/// started is killed with it (only that process).
pub struct Link {
    pub reader: Reader,
    pub writer: Writer,
    /// The transport authenticated the user (SSH stdio).
    pub local_auth: bool,
    _child: Option<Child>,
    _dir: Option<TunnelDir>,
}

/// An owner-only directory for a tunnel's socket, removed with the link.
struct TunnelDir(std::path::PathBuf);

impl TunnelDir {
    fn new() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!(
            "blongo-ssh-{}",
            crate::secret::b64(&crate::secret::random::<9>())
        ));
        std::fs::create_dir(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(dir))
    }
}

impl Drop for TunnelDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Remove tunnel folders a crashed Blongo left in the temp folder: ours
/// (same owner), older than a minute, whose socket no longer answers.
/// Returns how many were removed. Cheap; run once at startup.
pub fn clean_stale_tunnel_dirs() -> usize {
    clean_stale_tunnel_dirs_in(&std::env::temp_dir(), Duration::from_secs(60))
}

fn clean_stale_tunnel_dirs_in(tmp: &std::path::Path, min_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("blongo-ssh-") {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != unsafe { libc::geteuid() } {
                continue;
            }
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= min_age);
        if !old {
            continue;
        }
        #[cfg(unix)]
        if std::os::unix::net::UnixStream::connect(entry.path().join("t.sock")).is_ok() {
            continue; // a live tunnel of another Blongo
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

static SSH_OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Use this SSH client instead of `$BLONGO_SSH` / `ssh` (tests).
pub fn set_ssh_program(program: impl Into<String>) {
    *SSH_OVERRIDE.lock().expect("ssh override") = Some(program.into());
}

fn ssh_program() -> String {
    if let Some(p) = SSH_OVERRIDE.lock().expect("ssh override").clone() {
        return p;
    }
    std::env::var("BLONGO_SSH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ssh".into())
}

fn ssh_command(ssh_port: Option<u16>) -> Command {
    let mut cmd = Command::new(ssh_program());
    cmd.arg("-o").arg("BatchMode=yes");
    if let Some(p) = ssh_port {
        cmd.arg("-p").arg(p.to_string());
    }
    cmd.kill_on_drop(true);
    cmd
}

/// Keep the last bytes of a child's stderr for error messages.
fn drain_stderr(child: &mut Child) -> tokio::sync::watch::Receiver<String> {
    let (tx, rx) = tokio::sync::watch::channel(String::new());
    if let Some(mut err) = child.stderr.take() {
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let mut tail = String::new();
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                if tail.len() > 2048 {
                    let cut = tail.len() - 2048;
                    let cut = (cut..tail.len())
                        .find(|&i| tail.is_char_boundary(i))
                        .unwrap_or(tail.len());
                    tail.drain(..cut);
                }
                let _ = tx.send(tail.clone());
            }
        });
    }
    rx
}

pub async fn open(target: &Target) -> io::Result<Link> {
    tokio::time::timeout(CONNECT_TIMEOUT, open_inner(target))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))?
}

async fn open_inner(target: &Target) -> io::Result<Link> {
    match target {
        Target::WebSocket { host, port, path } => {
            let (reader, writer) = websocket(host, *port, path).await?;
            Ok(Link {
                reader,
                writer,
                local_auth: false,
                _child: None,
                _dir: None,
            })
        }
        Target::SshTunnel {
            host,
            ssh_port,
            remote_port,
        } => {
            // The forward listens on a Unix socket in a fresh owner-only
            // directory: no other local user (and no race for a free TCP
            // port) can reach or take the tunnel's local end.
            let dir = TunnelDir::new()?;
            let socket = dir.0.join("t.sock");
            let mut cmd = ssh_command(*ssh_port);
            cmd.arg("-N")
                .arg("-o")
                .arg("ExitOnForwardFailure=yes")
                .arg("-o")
                .arg("ServerAliveInterval=15")
                .arg("-o")
                .arg("StreamLocalBindMask=0177")
                .arg("-L")
                .arg(format!("{}:127.0.0.1:{remote_port}", socket.display()))
                .arg("--")
                .arg(host)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            let stderr = drain_stderr(&mut child);
            // Wait for the forward to accept connections.
            let mut waited = Duration::ZERO;
            loop {
                if let Some(status) = child.try_wait()? {
                    return Err(io::Error::other(format!(
                        "ssh exited ({status}): {}",
                        stderr.borrow().trim()
                    )));
                }
                if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await
                    && let Ok((reader, writer)) = websocket_on(stream, "localhost", 0, "/ws").await
                {
                    return Ok(Link {
                        reader,
                        writer,
                        local_auth: false,
                        _child: Some(child),
                        _dir: Some(dir),
                    });
                }
                if waited > CONNECT_TIMEOUT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the SSH tunnel did not come up",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += Duration::from_millis(100);
            }
        }
        Target::SshStdio {
            host,
            ssh_port,
            command,
        } => {
            let mut cmd = ssh_command(*ssh_port);
            cmd.arg("-T")
                .arg("--")
                .arg(host)
                .arg(command)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            let _ = drain_stderr(&mut child);
            let stdin = child.stdin.take().expect("piped");
            let stdout = child.stdout.take().expect("piped");
            let (reader, writer) = stream_halves(stdout, stdin, MAX_SERVER_FRAME);
            Ok(Link {
                reader,
                writer,
                local_auth: true,
                _child: Some(child),
                _dir: None,
            })
        }
    }
}

async fn websocket(host: &str, port: u16, path: &str) -> io::Result<(Reader, Writer)> {
    let addr_host = host.trim_start_matches('[').trim_end_matches(']');
    let stream = TcpStream::connect((addr_host, port)).await?;
    stream.set_nodelay(true)?;
    websocket_on(stream, host, port, path).await
}

async fn websocket_on<S>(
    stream: S,
    host: &str,
    port: u16,
    path: &str,
) -> io::Result<(Reader, Writer)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let url = format!("ws://{host}:{port}{path}");
    let (ws, _) = tokio_tungstenite::client_async_with_config(
        url.as_str(),
        stream,
        Some(ws_config(MAX_SERVER_FRAME)),
    )
    .await
    .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(ws_halves(ws))
}

#[cfg(test)]
mod tests {

    #[cfg(unix)]
    #[test]
    fn stale_tunnel_folders_are_removed_live_and_young_ones_kept() {
        let tmp = std::env::temp_dir().join(format!(
            "blongo-tunnels-{}",
            crate::secret::b64(&crate::secret::random::<6>())
        ));
        std::fs::create_dir_all(tmp.join("blongo-ssh-stale")).unwrap();
        std::fs::create_dir_all(tmp.join("blongo-ssh-live")).unwrap();
        std::fs::create_dir_all(tmp.join("other")).unwrap();
        let _live =
            std::os::unix::net::UnixListener::bind(tmp.join("blongo-ssh-live/t.sock")).unwrap();
        // Too young: kept.
        assert_eq!(
            clean_stale_tunnel_dirs_in(&tmp, Duration::from_secs(3600)),
            0
        );
        assert_eq!(clean_stale_tunnel_dirs_in(&tmp, Duration::ZERO), 1);
        assert!(!tmp.join("blongo-ssh-stale").exists());
        assert!(tmp.join("blongo-ssh-live").exists());
        assert!(tmp.join("other").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    use super::*;

    #[test]
    fn targets_parse_and_print() {
        let cases = [
            (
                "ws://127.0.0.1:7900",
                Target::WebSocket {
                    host: "127.0.0.1".into(),
                    port: 7900,
                    path: "/ws".into(),
                },
            ),
            (
                "ws://100.101.102.103",
                Target::WebSocket {
                    host: "100.101.102.103".into(),
                    port: DEFAULT_PORT,
                    path: "/ws".into(),
                },
            ),
            (
                "ws://[::1]:9/x",
                Target::WebSocket {
                    host: "[::1]".into(),
                    port: 9,
                    path: "/x".into(),
                },
            ),
            (
                "ssh://me@devbox?port=7901",
                Target::SshTunnel {
                    host: "me@devbox".into(),
                    ssh_port: None,
                    remote_port: 7901,
                },
            ),
            (
                "ssh+stdio://devbox:2222",
                Target::SshStdio {
                    host: "devbox".into(),
                    ssh_port: Some(2222),
                    command: DEFAULT_STDIO_COMMAND.into(),
                },
            ),
            (
                "ssh+stdio://devbox?command=/opt/blongo-serve%20--stdio",
                Target::SshStdio {
                    host: "devbox".into(),
                    ssh_port: None,
                    command: "/opt/blongo-serve --stdio".into(),
                },
            ),
        ];
        for (text, target) in cases {
            let parsed = Target::parse(text).unwrap();
            assert_eq!(parsed, target, "{text}");
            assert_eq!(Target::parse(&parsed.to_string()).unwrap(), target);
        }
        assert!(
            Target::parse("wss://x")
                .unwrap_err()
                .contains("not built in")
        );
        assert!(Target::parse("ftp://x").is_err());
        assert!(Target::parse("ws://:7").is_err());
        assert!(Target::parse("ssh://h?port=x").is_err());
        assert!(Target::parse("ssh+stdio://h").unwrap().is_local_auth());
        // Nothing ssh could read as an option, nor whitespace/control
        // characters, in the destination.
        for bad in [
            "ssh+stdio://-oProxyCommand=touch%20x",
            "ssh+stdio://-oProxyCommand=x",
            "ssh://-L1:2:3",
            "ssh://-evil@host",
            "ssh://me@-host",
            "ssh://me@@host",
            "ssh+stdio://ho st",
            "ssh+stdio://host\tx",
            "ssh+stdio://me@",
            "ws://ho st:1",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad} was accepted");
        }
        assert!(Target::parse("ssh://me.name@dev-box.lan").is_ok());
    }

    #[test]
    fn percent_decoding_is_lenient() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
