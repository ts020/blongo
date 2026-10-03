//! Managed install of Antigravity's ACP server (a prebuilt zip archive).
//!
//! Pinned to the 1.2.1 archives with the SHA-512 digests zeron pins (zeron
//! crates/harness/src/acp/mod.rs, MIT); the extraction rules follow zeron's
//! crates/harness/src/archive_install.rs. The download goes through a
//! `curl` subprocess (honours the user's proxy settings, adds no HTTP stack
//! to the binary). Layout under `root` (normally
//! `$XDG_DATA_HOME/blongo/antigravity-acp`):
//!
//! ```text
//! <version>/agy_acp_server.par   extracted, verified install
//! <version>/.blongo-ok           marker holding the archive digest
//! current -> <version>           symlink swapped atomically
//! ```
//!
//! Extraction happens in a `.tmp-*` sibling that is renamed into place only
//! after the digest matches and the entry exists, so a killed download never
//! passes for a working install.
//!
//! **Unverified against the real archive**: `dl.google.com` is unreachable
//! from the development environment; tests use a local HTTP server and a
//! synthetic archive.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use sha2::{Digest, Sha512};

use crate::acp::ANTIGRAVITY_ENTRY;

pub const ANTIGRAVITY_VERSION: &str = "1.2.1";
/// The only origin real installs download from.
pub const ANTIGRAVITY_ORIGIN: &str = "https://dl.google.com/";
const OK_MARKER: &str = ".blongo-ok";
const MAX_ARCHIVE_BYTES: u64 = 768 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 4_096;
/// A transfer slower than 1 byte/s for this long counts as stalled.
const STALL_SECS: &str = "60";

/// One pinned archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivePin {
    pub version: String,
    pub url: String,
    /// Path of the executable inside the archive.
    pub entry: String,
    pub sha512: String,
}

/// The pinned Antigravity build for this platform (`None` where Google
/// ships none).
pub fn antigravity_pin() -> Option<ArchivePin> {
    let (path, sha512) = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        (
            "macos/agy-acp-server-1.2.1-darwin-arm64.zip",
            "c0049b1f423ffdf0e6a79a6caa104a27b6368522b1eb2c67d9eeed1424af2a07898a66b475b0824a734a0bdd01264473a7d67d1550357a8607fb136507b0afae",
        )
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        (
            "macos/agy-acp-server-1.2.1-darwin-x86_64.zip",
            "790541c7e7bbbac1cb15bb4a77b02c53f30c997090487fbf42df16846e2e5d8d4f1cec92038d88956fbec33331ebacefe8faf974aad0690aadc2332215712ff2",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        (
            "linux/agy-acp-server-1.2.1-linux-x86_64.zip",
            "dd9b778479dbfc753661d63ff66a3235e7249e8451af12adc11d26509172de1ff50452c3af57684009bb944314bd587b2be200bab17df743a2706ce4067d6731",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        (
            "linux/agy-acp-server-1.2.1-linux-arm64.zip",
            "d907f928609ff2232bfeeec9177fe925d3e02e1773295cfb30097e5c4e134758021bff916ba5ae2e2e2ff81d374564349f268f92a4577bc88370537011473e29",
        )
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        (
            "windows/agy-acp-server-1.2.1-windows-x86_64.zip",
            "515c0af1d00164ca3a608f4ef04caa4ef8045ffa94a3ef2405c9b43c87c7a74c9f71a232c81917353dc3f2d49b69230f935d2ebfa28f7ee021b5aa088f3fa554",
        )
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        (
            "windows/agy-acp-server-1.2.1-windows-arm64.zip",
            "ae3f4c2d2255c03d4d9548e10a760a107c41c99e4c6c710bd82758791e4fd497b75bdef0cd2cfd7fdff1a5bb2fee7afd81f5d043da92ab52dce5505dd2aa58b3",
        )
    } else {
        return None;
    };
    Some(ArchivePin {
        version: ANTIGRAVITY_VERSION.into(),
        url: format!("{ANTIGRAVITY_ORIGIN}agy-extensions/releases/{path}"),
        entry: ANTIGRAVITY_ENTRY.into(),
        sha512: sha512.into(),
    })
}

/// `$XDG_DATA_HOME/blongo/antigravity-acp` (or `~/.local/share/...`).
pub fn default_root() -> Option<PathBuf> {
    crate::acp::antigravity_managed_entry()
        .and_then(|entry| entry.parent()?.parent().map(Path::to_path_buf))
}

/// The entry of a completed install of `pin` under `root`.
pub fn installed_entry(root: &Path, pin: &ArchivePin) -> Option<PathBuf> {
    let dir = root.join(&pin.version);
    if std::fs::read_to_string(dir.join(OK_MARKER)).ok()?.trim() != pin.sha512 {
        return None;
    }
    let entry = dir.join(&pin.entry);
    entry.is_file().then_some(entry)
}

/// Whether `url` is under `origin` (scheme, host and port must match
/// exactly: `https://dl.google.com.evil.test/` does not pass).
pub fn url_allowed(url: &str, origin: &str) -> bool {
    origin.ends_with('/') && url.starts_with(origin) && !url.contains("..")
}

/// Download, verify and unpack `pin` into `root`, then point `current` at
/// it. Returns the entry path. `allowed_origin` is
/// [`ANTIGRAVITY_ORIGIN`] in the app (tests pass a localhost origin).
/// `progress` receives human-readable status lines.
pub async fn install(
    root: &Path,
    pin: &ArchivePin,
    allowed_origin: &str,
    mut progress: impl FnMut(String),
) -> anyhow::Result<PathBuf> {
    if !url_allowed(&pin.url, allowed_origin) {
        anyhow::bail!(
            "refusing to download {} (not under {allowed_origin})",
            pin.url
        );
    }
    if let Some(entry) = installed_entry(root, pin) {
        point_current(root, &pin.version)?;
        return Ok(entry);
    }
    std::fs::create_dir_all(root)?;
    let tmp = root.join(format!(".tmp-{}-{}", pin.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let result = async {
        let archive = tmp.join("download.zip");
        progress(format!("Downloading Antigravity {}…", pin.version));
        download(&pin.url, &archive).await?;
        progress("Verifying the download…".into());
        let actual = sha512_file(archive.clone()).await?;
        if actual != pin.sha512 {
            anyhow::bail!(
                "SHA-512 mismatch for {} (expected {}, got {actual})",
                pin.url,
                pin.sha512
            );
        }
        progress("Unpacking…".into());
        let (a, d) = (archive.clone(), tmp.clone());
        tokio::task::spawn_blocking(move || extract_zip(&a, &d)).await??;
        std::fs::remove_file(&archive)?;
        if !tmp.join(&pin.entry).is_file() {
            anyhow::bail!("the archive has no {}", pin.entry);
        }
        mark_executables(&tmp)?;
        std::fs::write(tmp.join(OK_MARKER), format!("{}\n", pin.sha512))?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let final_dir = root.join(&pin.version);
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&tmp, &final_dir)?;
    point_current(root, &pin.version)?;
    installed_entry(root, pin).ok_or_else(|| anyhow::anyhow!("install did not resolve"))
}

async fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            // Only the pinned origin's scheme (https in the app; tests serve
            // plain http on localhost), and redirects only ever to https.
            "--proto",
            if url.starts_with("https://") {
                "=https"
            } else {
                "=https,http"
            },
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "30",
            "--speed-limit",
            "1",
            "--speed-time",
            STALL_SECS,
            "--max-filesize",
            &MAX_ARCHIVE_BYTES.to_string(),
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("could not run curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("download of {url} failed: {}", stderr.trim());
    }
    Ok(())
}

async fn sha512_file(path: PathBuf) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut digest = Sha512::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
        }
        Ok(format!("{:x}", digest.finalize()))
    })
    .await?
}

fn extract_zip(archive_path: &Path, dest: &Path) -> anyhow::Result<()> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(archive_path)?)?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        anyhow::bail!("archive has {} entries", archive.len());
    }
    let mut extracted = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        extracted = extracted.saturating_add(entry.size());
        if extracted > MAX_EXTRACTED_BYTES {
            anyhow::bail!("archive expands beyond the size limit");
        }
        let name = entry
            .enclosed_name()
            .ok_or_else(|| anyhow::anyhow!("archive contains an unsafe entry path"))?;
        if let Some(mode) = entry.unix_mode()
            && !matches!(mode & 0o170000, 0 | 0o040000 | 0o100000)
        {
            anyhow::bail!("archive entry {} is a link or special file", name.display());
        }
        let output = dest.join(name);
        if output == archive_path {
            anyhow::bail!("archive attempts to overwrite its download file");
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&output)?;
        let copied = std::io::copy(&mut (&mut entry).take(MAX_EXTRACTED_BYTES), &mut file)?;
        if copied != entry.size() {
            anyhow::bail!("archive entry {} is truncated", output.display());
        }
        file.flush()?;
    }
    Ok(())
}

/// Archives built off-unix can lose the executable bit; the entry spawns
/// sibling helpers.
#[cfg(unix)]
fn mark_executables(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let mut permissions = entry.metadata()?.permissions();
            permissions.set_mode(permissions.mode() | 0o755);
            std::fs::set_permissions(entry.path(), permissions)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn mark_executables(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Swap `current` to `version` atomically (symlink + rename).
#[cfg(unix)]
fn point_current(root: &Path, version: &str) -> std::io::Result<()> {
    let link = root.join("current");
    let tmp = root.join(format!(".current-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(version, &tmp)?;
    if link.is_dir() && !link.is_symlink() {
        std::fs::remove_dir_all(&link)?;
    }
    std::fs::rename(&tmp, &link)
}

#[cfg(not(unix))]
fn point_current(root: &Path, version: &str) -> std::io::Result<()> {
    // No portable atomic directory symlink: record the chosen version.
    std::fs::write(root.join("current.txt"), version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pinned_origin_passes() {
        let pin = antigravity_pin();
        if let Some(pin) = &pin {
            assert!(url_allowed(&pin.url, ANTIGRAVITY_ORIGIN));
            assert_eq!(pin.sha512.len(), 128);
        }
        for bad in [
            "https://dl.google.com.evil.test/agy-extensions/x.zip",
            "https://dl.google.com:8443/x.zip",
            "http://dl.google.com/x.zip",
            "https://dl.google.com/../x.zip",
        ] {
            assert!(!url_allowed(bad, ANTIGRAVITY_ORIGIN), "{bad}");
        }
    }
}
