//! A Mac app started from Finder or the Dock gets launchd's bare
//! environment (`PATH=/usr/bin:/bin:/usr/sbin:/sbin`), so `codex`,
//! `claude`, `node` and the user's other tools are not found. When running
//! from an app bundle, take the environment the user's login shell builds
//! (as a terminal would) before anything else starts.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = "__BLONGO_ENV__";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Variables that describe the shell itself, not the user's setup.
const SKIP: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", "TERM_PROGRAM"];

/// Load the login shell's environment when this executable lives inside
/// `*.app/Contents/MacOS/`. Must run before any other thread starts.
pub fn import_if_bundled() {
    let in_bundle = std::env::current_exe()
        .map(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false);
    if !in_bundle {
        return;
    }
    match login_shell_env() {
        Ok(vars) => apply(vars),
        Err(err) => eprintln!("blongo: cannot read the login shell's environment: {err}"),
    }
}

fn login_shell_env() -> Result<Vec<(String, String)>, String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into());
    // Interactive too: many people set PATH in .zshrc / .bashrc (nvm,
    // volta, ...). Whatever the rc files print lands before the marker.
    let mut child = Command::new(&shell)
        .args(["-l", "-i", "-c", &format!("printf '{MARKER}'; env -0")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{shell}: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{shell} did not finish in {TIMEOUT:?}"));
            }
        }
    }
    // Something the rc files started in the background can keep the pipe
    // open after the shell exits; don't wait for it.
    let out = rx
        .recv_timeout(TIMEOUT.saturating_sub(start.elapsed()) + Duration::from_millis(500))
        .map_err(|_| format!("{shell}: output did not end"))?;
    Ok(parse(&String::from_utf8_lossy(&out)))
}

fn parse(out: &str) -> Vec<(String, String)> {
    let Some((_, env)) = out.rsplit_once(MARKER) else {
        return Vec::new();
    };
    env.split('\0')
        .filter_map(|entry| entry.split_once('='))
        .filter(|(key, _)| !key.is_empty() && !SKIP.contains(key))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn apply(vars: Vec<(String, String)>) {
    for (key, value) in vars {
        // PATH always comes from the shell; anything launchd already set
        // (HOME, USER, TMPDIR, ...) stays.
        if key == "PATH" || std::env::var_os(&key).is_none() {
            // SAFETY: called from `main` before any other thread starts;
            // the pipe reader above has finished (it sent its output).
            unsafe { std::env::set_var(key, value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_after_the_marker_and_skips_shell_variables() {
        let out = format!("rc noise\n{MARKER}PATH=/opt/homebrew/bin:/usr/bin\0SHLVL=2\0A=b=c\0");
        assert_eq!(
            parse(&out),
            vec![
                ("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string()),
                ("A".to_string(), "b=c".to_string()),
            ]
        );
        assert!(parse("no marker").is_empty());
    }
}
