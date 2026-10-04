//! Owner-only files and folders (secrets, settings, tokens).

use std::io::Write;
use std::path::Path;

/// Make `path` a directory readable only by its owner (0700 on Unix).
///
/// Missing directories are created 0700. An existing one is tightened to
/// 0700 only when it belongs to this user and is not a shared directory
/// (sticky bit, like `/tmp`); a shared or foreign directory is refused,
/// never chmod-ed: Blongo must not change permissions of directories it
/// does not own, and its secrets do not belong in them.
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        if !path.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        let meta = std::fs::metadata(path)?;
        if !meta.is_dir() {
            return Err(std::io::Error::other(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        if meta.uid() != me || meta.mode() & 0o1000 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} is shared or belongs to another user; refusing to keep private files there",
                    path.display()
                ),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Replace `path` atomically with `data`, readable only by its owner
/// (0600 on Unix, set before any byte is written).
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // A missing parent is created private; an existing one is left as it
    // is (the file itself is 0600).
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        private_dir(dir)?;
    }
    let mut salt = [0u8; 6];
    getrandom::fill(&mut salt).expect("operating system randomness");
    let salt: String = salt.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = path.with_extension(format!("tmp-{salt}"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
