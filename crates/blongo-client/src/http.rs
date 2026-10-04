//! A minimal HTTP client: the system `curl` in a subprocess, configured
//! through its stdin (so tokens never appear in `ps` or the environment).
//! Request bodies travel inside that config too (`data-raw`), so nothing
//! is written to disk; `-q` comes first so a user's `~/.curlrc` cannot
//! change what is sent or where.
//!
//! Blongo talks HTTP only for opt-in features (the PR inbox, update
//! checks); a full HTTP/TLS stack in the binary would cost more memory
//! and size than these few requests justify. Plain `http://` is accepted
//! only for loopback addresses (tests' fake servers); everything else must
//! be `https://`, and curl is told to refuse redirects to anything else.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Most bytes of a response body read.
pub const MAX_BODY: usize = 16 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Largest request body (it is one line of curl's config, which curl
/// caps at 10 MiB).
pub const MAX_REQUEST_BODY: usize = 8 << 20;

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Request {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET",
            url: url.into(),
            ..Self::default()
        }
    }

    pub fn post_json(url: impl Into<String>, body: &serde_json::Value) -> Self {
        Self {
            method: "POST",
            url: url.into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Some(body.to_string().into_bytes()),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// `true` for `http://` URLs whose host is a loopback address.
fn loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Check a URL Blongo may fetch.
pub fn check_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || loopback_http(url) {
        if url.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
            return Err("the URL has characters curl's config cannot carry".into());
        }
        Ok(())
    } else {
        Err(format!(
            "refusing {url}: only https (or http on loopback) is allowed"
        ))
    }
}

/// One curl config value, quoted (curl's escapes: `\\`, `\"`, `\n`, …).
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The config text curl reads from stdin for `req`, body included.
fn config(req: &Request) -> Result<String, String> {
    check_url(&req.url)?;
    let mut c = String::new();
    c.push_str("silent\nshow-error\n");
    c.push_str(&format!("max-time = {}\n", TIMEOUT.as_secs()));
    c.push_str(&format!("max-filesize = {MAX_BODY}\n"));
    c.push_str("proto = \"=https,http\"\nproto-redir = \"=https\"\n");
    c.push_str(&format!("request = {}\n", quote(req.method)));
    c.push_str(&format!("url = {}\n", quote(&req.url)));
    if loopback_http(&req.url) {
        c.push_str("noproxy = \"*\"\n");
    }
    for (name, value) in &req.headers {
        if name.contains([':', '\n', '\r']) || value.contains(['\n', '\r']) {
            return Err(format!("bad header {name}"));
        }
        c.push_str(&format!(
            "header = {}\n",
            quote(&format!("{name}: {value}"))
        ));
    }
    c.push_str("header = \"User-Agent: blongo\"\n");
    if let Some(body) = &req.body {
        // Text only (JSON): a config value cannot carry NUL or other
        // control bytes, and `data-raw` never reads a file for `@`.
        let text = std::str::from_utf8(body).map_err(|_| "the request body is not text")?;
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            return Err("the request body has control characters".into());
        }
        // The quoted value is what curl reads as one config line (its
        // limit is 10 MiB), so the cap applies after escaping.
        let quoted = quote(text);
        if quoted.len() > MAX_REQUEST_BODY {
            return Err("the request body is too large".into());
        }
        c.push_str(&format!("data-raw = {quoted}\n"));
    }
    // The status code on a line of its own after the body.
    c.push_str("write-out = \"\\n%{http_code}\"\n");
    Ok(c)
}

/// Send one request. Errors: curl missing, network failures, a refused
/// URL; HTTP error statuses are a [`Response`] like any other.
pub async fn send(req: Request) -> Result<Response, String> {
    run(&req).await
}

async fn run(req: &Request) -> Result<Response, String> {
    let config = config(req)?;
    let program = std::env::var("BLONGO_CURL").unwrap_or_else(|_| "curl".into());
    let mut child = tokio::process::Command::new(&program)
        .args(["-q", "--config", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("curl stdin")?;
    stdin
        .write_all(config.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let mut stdout = child.stdout.take().ok_or("curl stdout")?;
    let mut stderr = child.stderr.take().ok_or("curl stderr")?;
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(8192).read_to_end(&mut buf).await;
        buf
    });
    let mut out = Vec::new();
    (&mut stdout)
        .take(MAX_BODY as u64 + 16)
        .read_to_end(&mut out)
        .await
        .map_err(|e| e.to_string())?;
    let status = tokio::time::timeout(TIMEOUT + Duration::from_secs(5), child.wait())
        .await
        .map_err(|_| "curl did not finish".to_owned())?
        .map_err(|e| e.to_string())?;
    let err = err_task.await.unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "request to {} failed: {}",
            req.url,
            String::from_utf8_lossy(&err).trim()
        ));
    }
    let split = out
        .iter()
        .rposition(|b| *b == b'\n')
        .ok_or("curl gave no status")?;
    let code = std::str::from_utf8(&out[split + 1..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or("curl gave no status")?;
    out.truncate(split);
    Ok(Response {
        status: code,
        body: out,
    })
}

/// Download `url` into `dest` (created 0600), at most `max` bytes.
pub async fn download(url: &str, dest: &std::path::Path, max: u64) -> Result<u64, String> {
    check_url(url)?;
    // A fresh 0600 file (the caller's folder is Blongo's own private one;
    // no directory permissions are touched here).
    match std::fs::remove_file(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", dest.display())),
    }
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(dest)
            .map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    let mut c = config(&Request::get(url))?;
    c = c.replace(
        &format!("max-filesize = {MAX_BODY}\n"),
        &format!("max-filesize = {max}\n"),
    );
    c = c.replace(
        &format!("max-time = {}\n", TIMEOUT.as_secs()),
        "max-time = 600\n",
    );
    c.push_str("fail\n");
    c.push_str(&format!(
        "output = {}\n",
        quote(&dest.display().to_string())
    ));
    c = c.replace("write-out = \"\\n%{http_code}\"\n", "");
    let program = std::env::var("BLONGO_CURL").unwrap_or_else(|_| "curl".into());
    let mut child = tokio::process::Command::new(&program)
        .args(["-q", "--config", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("curl stdin")?;
    stdin
        .write_all(c.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    drop(stdin);
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "download of {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::metadata(dest)
        .map(|m| m.len())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_loopback_http() {
        assert!(check_url("https://api.github.com/x").is_ok());
        assert!(check_url("http://127.0.0.1:8080/x").is_ok());
        assert!(check_url("http://[::1]:9/x").is_ok());
        assert!(check_url("http://localhost/x").is_ok());
        assert!(check_url("http://example.com/x").is_err());
        assert!(check_url("http://127.0.0.1.evil.com/x").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
        assert!(check_url("https://x/\"\nurl = file:///etc").is_err());
    }

    #[test]
    fn config_quotes_values_and_never_takes_headers_with_newlines() {
        let req = Request::get("https://h/x").header("Authorization", "Bearer a\"b\\c");
        let c = config(&req).unwrap();
        assert!(
            c.contains(r#"header = "Authorization: Bearer a\"b\\c""#),
            "{c}"
        );
        let bad = Request::get("https://h/x").header("X", "a\nurl = file:///etc/passwd");
        assert!(config(&bad).is_err());
        // Bodies go inside the config, quoted; binary bodies are refused.
        let post = Request::post_json("https://h/x", &serde_json::json!({"a": "q\"\\\nz"}));
        let c = config(&post).unwrap();
        assert!(
            c.contains(r#"data-raw = "{\"a\":\"q\\\"\\\\\\nz\"}""#),
            "{c}"
        );
        let mut binary = Request::get("https://h/x");
        binary.body = Some(vec![b'a', 0, b'b']);
        assert!(config(&binary).is_err());
        // The cap counts the escaped value: 5 MiB of quotes become 10 MiB.
        let mut quotes = Request::get("https://h/x");
        quotes.body = Some(vec![b'"'; 5 << 20]);
        assert_eq!(
            config(&quotes).unwrap_err(),
            "the request body is too large"
        );
        let mut plain = Request::get("https://h/x");
        plain.body = Some(vec![b'a'; 5 << 20]);
        assert!(config(&plain).is_ok());
    }
}
