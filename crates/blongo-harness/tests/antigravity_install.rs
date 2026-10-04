//! The Antigravity archive install against a local HTTP server and a
//! synthetic archive. The real archive (dl.google.com) is unreachable from
//! the development environment, so the pinned 1.2.1 digests themselves are
//! unverified here; these tests pin the mechanics: origin check, download
//! through curl, SHA-512 verification, safe extraction, the executable bit,
//! the `current` link and idempotence.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use blongo_harness::antigravity_install::{ArchivePin, install, installed_entry};
use sha2::{Digest, Sha512};
use tokio::io::AsyncBufReadExt;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "blongo-install-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

fn sha512(path: &Path) -> String {
    format!("{:x}", Sha512::digest(std::fs::read(path).unwrap()))
}

/// `python3 -m http.server` on an ephemeral localhost port, serving `dir`.
struct Server {
    child: tokio::process::Child,
    origin: String,
}

impl Server {
    async fn start(dir: &Path) -> Self {
        let script = "import http.server, functools, sys\n\
             class Quiet(http.server.SimpleHTTPRequestHandler):\n\
             \x20   def log_message(self, *a): pass\n\
             h = functools.partial(Quiet, directory=sys.argv[1])\n\
             s = http.server.HTTPServer(('127.0.0.1', 0), h)\n\
             print(s.server_address[1], flush=True)\n\
             s.serve_forever()\n";
        let mut child = tokio::process::Command::new("python3")
            .args(["-c", script])
            .arg(dir)
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let port = lines.next_line().await.unwrap().unwrap();
        Self {
            child,
            origin: format!("http://127.0.0.1:{}/", port.trim()),
        }
    }

    async fn stop(mut self) {
        // Our own child, by handle.
        let _ = self.child.kill().await;
    }
}

#[tokio::test]
async fn installs_verifies_and_links_current() {
    let serve = scratch("serve");
    let archive = serve.join("agy.zip");
    write_zip(
        &archive,
        &[
            ("agy_acp_server.par", b"#!/bin/sh\necho fake\n"),
            ("localharness_external", b"helper"),
        ],
    );
    let server = Server::start(&serve).await;
    let root = scratch("root");
    let pin = ArchivePin {
        version: "9.9.9".into(),
        url: format!("{}agy.zip", server.origin),
        entry: "agy_acp_server.par".into(),
        sha512: sha512(&archive),
    };
    let mut steps = Vec::new();
    let entry = install(&root, &pin, &server.origin, |s| steps.push(s))
        .await
        .unwrap();
    assert_eq!(entry, root.join("9.9.9/agy_acp_server.par"));
    assert_eq!(std::fs::read(&entry).unwrap(), b"#!/bin/sh\necho fake\n");
    assert!(steps.iter().any(|s| s.starts_with("Downloading")));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&entry).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "entry must be executable");
        assert_eq!(
            std::fs::read_link(root.join("current")).unwrap(),
            PathBuf::from("9.9.9")
        );
        assert!(root.join("current/agy_acp_server.par").is_file());
    }
    assert_eq!(installed_entry(&root, &pin), Some(entry.clone()));
    // No temp dirs or download left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(!root.join("9.9.9/download.zip").exists());

    // Idempotent: a second install does not download again.
    std::fs::remove_file(&archive).unwrap();
    let again = install(&root, &pin, &server.origin, |_| {}).await.unwrap();
    assert_eq!(again, entry);
    server.stop().await;
    std::fs::remove_dir_all(&serve).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn rejects_bad_digest_wrong_origin_missing_entry_and_404() {
    let serve = scratch("serve-bad");
    let archive = serve.join("agy.zip");
    write_zip(&archive, &[("something_else", b"x")]);
    let server = Server::start(&serve).await;
    let root = scratch("root-bad");
    let good_digest = sha512(&archive);
    let pin = |url: String, sha512: String| ArchivePin {
        version: "1.0.0".into(),
        url,
        entry: "agy_acp_server.par".into(),
        sha512,
    };

    let err = install(
        &root,
        &pin(format!("{}agy.zip", server.origin), "00".repeat(64)),
        &server.origin,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("SHA-512 mismatch"), "{err}");

    let err = install(
        &root,
        &pin(format!("{}agy.zip", server.origin), good_digest.clone()),
        "https://dl.google.com/",
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("refusing"), "{err}");

    let err = install(
        &root,
        &pin(format!("{}agy.zip", server.origin), good_digest.clone()),
        &server.origin,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("has no agy_acp_server.par"),
        "{err}"
    );

    let err = install(
        &root,
        &pin(format!("{}missing.zip", server.origin), good_digest),
        &server.origin,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("download"), "{err}");

    // Every failure cleaned up after itself.
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    server.stop().await;
    std::fs::remove_dir_all(&serve).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn rejects_path_traversal_entries() {
    let serve = scratch("serve-evil");
    let archive = serve.join("evil.zip");
    write_zip(
        &archive,
        &[("agy_acp_server.par", b"x"), ("../escape.txt", b"bad")],
    );
    let server = Server::start(&serve).await;
    let root = scratch("root-evil");
    let pin = ArchivePin {
        version: "1.0.0".into(),
        url: format!("{}evil.zip", server.origin),
        entry: "agy_acp_server.par".into(),
        sha512: sha512(&archive),
    };
    let err = install(&root, &pin, &server.origin, |_| {})
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unsafe"), "{err}");
    assert!(!root.parent().unwrap().join("escape.txt").exists());
    server.stop().await;
    std::fs::remove_dir_all(&serve).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}
