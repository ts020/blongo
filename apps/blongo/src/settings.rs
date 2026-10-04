//! The app's settings file (`settings.json` in the config dir, 0600) and
//! the global that holds it while the window is open.
//!
//! What the core needs (approval policy, default models) is pushed to the
//! local core with `Backend::configure` whenever it changes; the generic
//! ACP agent's command line is read once at start (the core's executables
//! are fixed while it runs).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blongo_protocol::ProviderKind;
use blongo_protocol::client::{ApprovalPolicy, CoreSettings};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

/// When a desktop notification is shown for a finished run or a pending
/// approval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyMode {
    Off,
    /// Only while the window is not focused.
    #[default]
    Unfocused,
    Always,
}

/// A generic Agent Client Protocol agent: the command that starts it in
/// ACP mode (e.g. `opencode acp`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AcpCommand {
    pub executable: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub approval: ApprovalPolicy,
    pub theme: ThemeMode,
    /// Provider new threads start with.
    pub default_provider: ProviderKind,
    /// Provider id → model id new threads of that provider start with.
    pub default_models: BTreeMap<String, String>,
    pub notifications: NotifyMode,
    pub acp: AcpCommand,
    /// Check for an update when the app starts (only when a signed update
    /// channel is configured).
    pub check_updates: bool,
}

impl AppSettings {
    pub fn core(&self) -> CoreSettings {
        CoreSettings {
            approval: self.approval,
            default_models: self
                .default_models
                .iter()
                .filter(|(_, m)| !m.trim().is_empty())
                .filter_map(|(p, m)| Some((ProviderKind::parse(p)?, m.trim().to_owned())))
                .collect(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("{} is not valid: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            blongo_client::secret::private_dir(dir).map_err(|e| e.to_string())?;
        }
        blongo_client::secret::write_private(path, &json)
            .map_err(|e| format!("cannot save {}: {e}", path.display()))
    }
}

pub fn settings_path() -> PathBuf {
    blongo_client::environments::config_dir().join("settings.json")
}

pub fn keybindings_path() -> PathBuf {
    blongo_client::environments::config_dir().join("keybindings.json")
}

/// The settings while the app runs, plus where they live.
pub struct Settings {
    pub path: PathBuf,
    pub value: AppSettings,
    /// Why the file could not be read (defaults are used meanwhile and the
    /// file is not overwritten until the user changes something).
    pub load_error: Option<String>,
}

impl gpui::Global for Settings {}

impl Settings {
    pub fn load() -> Self {
        let path = settings_path();
        match AppSettings::load(&path) {
            Ok(value) => Self {
                path,
                value,
                load_error: None,
            },
            Err(err) => Self {
                path,
                value: AppSettings::default(),
                load_error: Some(err),
            },
        }
    }

    pub fn save(&mut self) -> Result<(), String> {
        let result = self.value.save(&self.path);
        if result.is_ok() {
            self.load_error = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_missing_fields() {
        let dir = std::env::temp_dir().join(format!("blongo-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        assert_eq!(AppSettings::load(&path).unwrap(), AppSettings::default());
        let mut s = AppSettings {
            approval: ApprovalPolicy::AutoApprove,
            theme: ThemeMode::Light,
            notifications: NotifyMode::Always,
            ..AppSettings::default()
        };
        s.default_models.insert("codex".into(), "fast".into());
        s.default_models.insert("claude-code".into(), "  ".into());
        s.default_models.insert("nonsense".into(), "x".into());
        s.save(&path).unwrap();
        assert_eq!(AppSettings::load(&path).unwrap(), s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Blank and unknown entries do not reach the core.
        assert_eq!(
            s.core().default_models,
            vec![(ProviderKind::Codex, "fast".to_owned())]
        );
        std::fs::write(&path, br#"{"theme":"light"}"#).unwrap();
        let partial = AppSettings::load(&path).unwrap();
        assert_eq!(partial.theme, ThemeMode::Light);
        assert_eq!(partial.notifications, NotifyMode::Unfocused);
        std::fs::write(&path, b"{not json").unwrap();
        assert!(AppSettings::load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
