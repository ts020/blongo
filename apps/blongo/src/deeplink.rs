//! `blongo://` links: parsing, handing a link to the running instance, and
//! registering the handler.
//!
//! Links:
//! - `blongo://thread/<uuid>`: open a thread (in the local core).
//! - `blongo://project?path=/abs/path`: add (or reuse) a project for a
//!   folder and start a thread in it.
//! - `blongo://settings`, `blongo://inbox`: open those views.
//!
//! The OS starts `blongo <url>`. When an instance is already running, the
//! new process writes the link to the instance's socket
//! (`<data_dir>/app.sock`, 0600 in a 0700 folder) and exits; otherwise it
//! starts normally and opens the link once the shell is loaded. A link is
//! only ever *navigation*: it never sends a prompt or approves anything.
//!
//! Registering: `blongo register-url-handler` (Linux) writes a desktop
//! entry with `MimeType=x-scheme-handler/blongo` and runs `xdg-mime`.
//! macOS and Windows: see docs/phase4/deep-links.md.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use blongo_protocol::ThreadId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Thread(ThreadId),
    Project(String),
    Settings,
    Inbox,
}

/// Longest link accepted (anything longer is not one of ours).
const MAX_LEN: usize = 4096;

pub fn parse(url: &str) -> Result<Link, String> {
    let url = url.trim();
    if url.len() > MAX_LEN {
        return Err("the link is too long".into());
    }
    let rest = url
        .strip_prefix("blongo://")
        .ok_or_else(|| format!("not a blongo:// link: {url}"))?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = path.trim_end_matches('/');
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("thread"), Some(id), None) => ThreadId::parse(id)
            .map(Link::Thread)
            .ok_or_else(|| format!("not a thread id: {id}")),
        (Some("project"), None, None) => {
            let path = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "path")
                .map(|(_, v)| percent_decode(v))
                .transpose()?
                .ok_or("the project link has no path")?;
            if !Path::new(&path).is_absolute() {
                return Err(format!("the project path must be absolute: {path}"));
            }
            Ok(Link::Project(path))
        }
        (Some("settings"), None, None) => Ok(Link::Settings),
        (Some("inbox"), None, None) => Ok(Link::Inbox),
        _ => Err(format!("unknown blongo:// link: {url}")),
    }
}

fn percent_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or("bad percent escape in the link")?;
                out.push(hex);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    let text = String::from_utf8(out).map_err(|_| "the link is not UTF-8")?;
    if text.contains('\0') {
        return Err("the link contains a NUL".into());
    }
    Ok(text)
}

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("app.sock")
}

/// Hand `url` to a running instance. `Ok(true)`: it took it.
#[cfg(unix)]
pub fn forward(data_dir: &Path, url: &str) -> std::io::Result<bool> {
    use std::os::unix::net::UnixStream;
    let mut stream = match UnixStream::connect(socket_path(data_dir)) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.write_all(url.trim().replace('\n', "").as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(true)
}

#[cfg(not(unix))]
pub fn forward(_data_dir: &Path, _url: &str) -> std::io::Result<bool> {
    Ok(false)
}

/// Listen for links from later `blongo <url>` processes; each line read is
/// passed to `on_link`. The listener thread ends with the process.
#[cfg(unix)]
pub fn listen(data_dir: &Path, on_link: impl Fn(String) + Send + 'static) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    blongo_client::secret::private_dir(data_dir)?;
    let path = socket_path(data_dir);
    // A socket left by a crashed instance: nobody answers it.
    if path.exists() && UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new()
        .name("blongo-links".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let mut line = String::new();
                let mut reader = BufReader::new(std::io::Read::take(stream, MAX_LEN as u64 + 2));
                if reader.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                    on_link(line.trim().to_owned());
                }
            }
        })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn listen(_data_dir: &Path, _on_link: impl Fn(String) + Send + 'static) -> std::io::Result<()> {
    Ok(())
}

/// Remove the socket when the app exits.
pub fn unlisten(data_dir: &Path) {
    let _ = std::fs::remove_file(socket_path(data_dir));
}

/// Linux: a desktop entry that makes `blongo` the `blongo://` handler.
/// `None` for a path a desktop entry cannot hold (control characters).
pub fn desktop_entry(exe: &Path) -> Option<String> {
    let exe = exe.display().to_string();
    if exe.chars().any(char::is_control) {
        return None;
    }
    // Inside a quoted Exec argument `"`, `` ` ``, `$` and `\` take a
    // backslash and `%` is doubled; then the whole value is escaped as a
    // desktop-entry string, where a backslash is written `\\`.
    let mut quoted = String::new();
    for c in exe.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            c => quoted.push(c),
        }
    }
    let value = quoted.replace('\\', "\\\\");
    Some(format!(
        "[Desktop Entry]\nType=Application\nName=Blongo\nExec=\"{value}\" %u\n\
         Terminal=false\nNoDisplay=true\nMimeType=x-scheme-handler/blongo;\n"
    ))
}

/// `blongo register-url-handler`: write the desktop entry and make it the
/// default handler (Linux).
pub fn register() -> Result<String, String> {
    if !cfg!(all(unix, not(target_os = "macos"))) {
        return Err(
            "on this OS the handler is registered by the app bundle / installer; \
             see docs/phase4/deep-links.md"
                .into(),
        );
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = dirs::data_dir()
        .ok_or("no data directory")?
        .join("applications");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("blongo-url-handler.desktop");
    let entry = desktop_entry(&exe)
        .ok_or("the program's path has characters a desktop entry cannot hold")?;
    std::fs::write(&file, entry).map_err(|e| e.to_string())?;
    let status = std::process::Command::new("xdg-mime")
        .args([
            "default",
            "blongo-url-handler.desktop",
            "x-scheme-handler/blongo",
        ])
        .status();
    Ok(match status {
        Ok(s) if s.success() => format!("registered {}", file.display()),
        _ => format!(
            "wrote {}; run `xdg-mime default blongo-url-handler.desktop \
             x-scheme-handler/blongo` to make it the default",
            file.display()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_parse() {
        let id = ThreadId::new();
        assert_eq!(
            parse(&format!("blongo://thread/{id}")),
            Ok(Link::Thread(id))
        );
        assert_eq!(
            parse(&format!("blongo://thread/{id}/")),
            Ok(Link::Thread(id))
        );
        assert_eq!(
            parse("blongo://project?path=%2Fhome%2Fme%2Fmy%20app"),
            Ok(Link::Project("/home/me/my app".into()))
        );
        assert_eq!(parse("blongo://settings"), Ok(Link::Settings));
        assert_eq!(parse(" blongo://inbox\n"), Ok(Link::Inbox));
        assert!(parse("blongo://thread/nope").is_err());
        assert!(parse("blongo://project?path=relative").is_err());
        assert!(parse("blongo://project").is_err());
        assert!(parse("blongo://project?path=%2Fa%00b").is_err());
        assert!(parse("blongo://project?path=%zz").is_err());
        assert!(parse("https://example.com").is_err());
        assert!(parse("blongo://run/rm -rf").is_err());
        assert!(parse(&format!("blongo://settings?{}", "a".repeat(5000))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_second_process_hands_its_link_over() {
        let dir = std::env::temp_dir().join(format!("blongo-links-{}", std::process::id()));
        assert!(!forward(&dir, "blongo://settings").unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        listen(&dir, move |l| tx.send(l).unwrap()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(socket_path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(forward(&dir, "blongo://inbox").unwrap());
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            "blongo://inbox"
        );
        unlisten(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_entry_names_the_scheme() {
        let entry = desktop_entry(Path::new("/opt/blongo/blongo")).unwrap();
        assert!(entry.contains("Exec=\"/opt/blongo/blongo\" %u"));
        assert!(entry.contains("MimeType=x-scheme-handler/blongo;"));
        // Exec quoting, then string escaping: `$` → `\$` → `\\$`; `%` → `%%`.
        let entry = desktop_entry(Path::new("/opt/a\"b$c`d\\e%f/blongo")).unwrap();
        assert!(
            entry.contains(r#"Exec="/opt/a\\"b\\$c\\`d\\\\e%%f/blongo" %u"#),
            "{entry}"
        );
        assert!(desktop_entry(Path::new("/opt/x\ny/blongo")).is_none());
    }
}
