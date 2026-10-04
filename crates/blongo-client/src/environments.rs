//! The client's list of remote environments and their credentials, kept in
//! `environments.json` in the config directory (`$BLONGO_CONFIG_DIR`,
//! default: the platform config dir + `blongo`), owner-only (0600).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::secret::{private_dir, write_private};
use crate::target::Target;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EnvironmentFile {
    pub environments: Vec<Environment>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Environment {
    pub name: String,
    /// See [`Target`].
    pub target: String,
    /// `None` for transports that authenticate by themselves (SSH stdio).
    pub credential: Option<Credential>,
}

impl Environment {
    pub fn target(&self) -> Result<Target, String> {
        Target::parse(&self.target)
    }
}

/// What pairing issued: the server it belongs to, the device id, the bearer
/// token, and the device key the token is bound to.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credential {
    pub server_id: String,
    pub device_id: String,
    pub token: String,
    /// Ed25519 secret key (URL-safe base64 of the 32-byte seed).
    pub device_key: String,
}

/// Never print secrets.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("server_id", &self.server_id)
            .field("device_id", &self.device_id)
            .field("token", &"<redacted>")
            .field("device_key", &"<redacted>")
            .finish()
    }
}

pub fn config_dir() -> PathBuf {
    blongo_forge::config_dir()
}

pub fn default_path() -> PathBuf {
    config_dir().join("environments.json")
}

impl EnvironmentFile {
    /// Missing file: no environments.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("cannot read {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            private_dir(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_vec_pretty(self).expect("serializable");
        write_private(path, &json).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Load, change and save under an exclusive lock (`<file>.lock`), so
    /// the app and `blongo env` editing at once do not lose an entry.
    pub fn update<R>(path: &Path, change: impl FnOnce(&mut Self) -> R) -> Result<R, String> {
        if let Some(dir) = path.parent() {
            private_dir(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let lock_path = path.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
        lock.lock()
            .map_err(|e| format!("cannot lock {}: {e}", lock_path.display()))?;
        let mut file = Self::load(path)?;
        let result = change(&mut file);
        file.save(path)?;
        Ok(result)
    }

    pub fn get(&self, name: &str) -> Option<&Environment> {
        self.environments.iter().find(|e| e.name == name)
    }

    /// Add or replace the environment with this name.
    pub fn upsert(&mut self, env: Environment) {
        match self.environments.iter_mut().find(|e| e.name == env.name) {
            Some(slot) => *slot = env,
            None => self.environments.push(env),
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.environments.len();
        self.environments.retain(|e| e.name != name);
        before != self.environments.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_privately_and_redacts() {
        let dir = std::env::temp_dir().join(format!(
            "blongo-envs-{}",
            crate::secret::b64(&crate::secret::random::<6>())
        ));
        let path = dir.join("environments.json");
        assert!(
            EnvironmentFile::load(&path)
                .unwrap()
                .environments
                .is_empty()
        );
        let cred = Credential {
            server_id: "s".into(),
            device_id: "d".into(),
            token: "super-secret-token".into(),
            device_key: "super-secret-key".into(),
        };
        let mut file = EnvironmentFile::default();
        file.upsert(Environment {
            name: "devbox".into(),
            target: "ws://127.0.0.1:7878/ws".into(),
            credential: Some(cred.clone()),
        });
        file.upsert(Environment {
            name: "devbox".into(),
            target: "ws://127.0.0.1:7879/ws".into(),
            credential: Some(cred.clone()),
        });
        file.save(&path).unwrap();
        let back = EnvironmentFile::load(&path).unwrap();
        assert_eq!(back.environments.len(), 1);
        assert_eq!(back.get("devbox").unwrap().target, "ws://127.0.0.1:7879/ws");
        let debug = format!("{back:?}");
        assert!(!debug.contains("super-secret"), "{debug}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let mut back = back;
        assert!(back.remove("devbox"));
        assert!(!back.remove("devbox"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
