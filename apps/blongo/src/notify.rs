//! Desktop notifications when a run finishes or an agent waits for an
//! approval.
//!
//! Linux: `notify-send` (libnotify), else `gdbus` calling
//! `org.freedesktop.Notifications.Notify` directly; both are tiny
//! processes started on demand, so nothing stays loaded. macOS:
//! `osascript -e 'display notification …'`. Windows: not implemented (see
//! docs/phase4/notifications.md). `BLONGO_NOTIFY_CMD` replaces all of them:
//! it is run with the title and body as its two arguments (tests use it).

use std::process::{Command, Stdio};

/// Show a notification without waiting for it. The process is reaped on a
/// short-lived thread.
pub fn send(title: &str, body: &str) {
    let Some(mut command) = command(title, body) else {
        return;
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("blongo-notify".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(err) => eprintln!("blongo: cannot show a notification: {err}"),
    }
}

fn command(title: &str, body: &str) -> Option<Command> {
    command_from(std::env::var_os("BLONGO_NOTIFY_CMD"), title, body)
}

fn command_from(custom: Option<std::ffi::OsString>, title: &str, body: &str) -> Option<Command> {
    if let Some(cmd) = custom.filter(|c| !c.is_empty()) {
        let mut c = Command::new(cmd);
        c.arg(title).arg(body);
        return Some(c);
    }
    platform(title, body)
}

#[cfg(target_os = "macos")]
fn platform(title: &str, body: &str) -> Option<Command> {
    let mut c = Command::new("osascript");
    c.arg("-e").arg(format!(
        "display notification {} with title {}",
        applescript_string(body),
        applescript_string(title)
    ));
    Some(c)
}

#[cfg(target_os = "macos")]
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform(title: &str, body: &str) -> Option<Command> {
    if let Some(path) = which("notify-send") {
        let mut c = Command::new(path);
        c.args(["--app-name=Blongo", "--", title, body]);
        return Some(c);
    }
    let path = which("gdbus")?;
    let mut c = Command::new(path);
    c.args(gdbus_args(title, body));
    Some(c)
}

#[cfg(not(unix))]
fn platform(_title: &str, _body: &str) -> Option<Command> {
    None
}

/// `gdbus call` arguments for the freedesktop notification service.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn gdbus_args(title: &str, body: &str) -> Vec<String> {
    let gv = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
    vec![
        "call".into(),
        "--session".into(),
        "--dest=org.freedesktop.Notifications".into(),
        "--object-path=/org/freedesktop/Notifications".into(),
        "--method=org.freedesktop.Notifications.Notify".into(),
        "Blongo".into(),
        "0".into(),
        "".into(),
        gv(title),
        gv(body),
        "[]".into(),
        "{}".into(),
        "5000".into(),
    ]
}

#[cfg(unix)]
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdbus_strings_are_quoted() {
        let args = gdbus_args("Run finished", "it's \\ done");
        assert_eq!(args[8], "'Run finished'");
        assert_eq!(args[9], r"'it\'s \\ done'");
    }

    #[cfg(unix)]
    #[test]
    fn override_command_gets_title_and_body() {
        let dir = std::env::temp_dir().join(format!("blongo-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let script = dir.join("n.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|%s' \"$1\" \"$2\" > {}\n",
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut c = command_from(Some(script.into()), "T", "B c").unwrap();
        assert!(c.status().unwrap().success());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "T|B c");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
