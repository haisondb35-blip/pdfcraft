//! What the person chose: syncing on or off, the folder, how many versions to keep. One file for
//! every Craft app, `ArtCraft/sync.json` in the system's settings folder (`%APPDATA%` on Windows),
//! so turning it on in one app turns it on in all of them.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Versions kept of each file when nothing was chosen.
pub const DEFAULT_VERSIONS: usize = 20;
/// The most versions that can be chosen.
pub const MAX_VERSIONS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Keep the shared settings and AI conversations in the cloud folder too.
    pub enabled: bool,
    /// The folder chosen; none: [`crate::cloud::default_folder`].
    pub folder: Option<PathBuf>,
    /// Earlier versions kept of each saved file (0: none).
    pub keep_versions: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: false, folder: None, keep_versions: DEFAULT_VERSIONS }
    }
}

/// `ArtCraft` in the system's settings folder, the folder every Craft app shares. None with
/// `ARTCRAFT_NO_AI_MEMORY` set (test runs keep nothing).
pub fn base_dir() -> Option<PathBuf> {
    if std::env::var_os("ARTCRAFT_NO_AI_MEMORY").is_some() {
        return None;
    }
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support"))
    } else if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
    }?;
    Some(base.join("ArtCraft"))
}

impl Settings {
    /// The saved settings (the defaults when there are none or they can't be read).
    pub fn load() -> Settings {
        base_dir().map(|d| Settings::load_from(&d.join("sync.json"))).unwrap_or_default()
    }

    pub fn load_from(file: &Path) -> Settings {
        let Some(v) = std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return Settings::default() };
        Settings {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            folder: v.get("folder").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(PathBuf::from),
            keep_versions: v
                .get("keepVersions")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_VERSIONS, |n| usize::try_from(n).unwrap_or(MAX_VERSIONS).min(MAX_VERSIONS)),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = base_dir().ok_or("no settings folder")?;
        self.save_to(&dir.join("sync.json"))
    }

    pub fn save_to(&self, file: &Path) -> Result<(), String> {
        let v = json!({
            "enabled": self.enabled,
            "folder": self.folder.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "keepVersions": self.keep_versions.min(MAX_VERSIONS),
        });
        let dir = file.parent().ok_or("no folder")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, v.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, file).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// The cloud folder in use: the one chosen, else `ArtCraft` in iCloud Drive (or another cloud).
    pub fn folder(&self) -> Option<PathBuf> {
        self.folder.clone().or_else(crate::cloud::default_folder)
    }

    /// The cloud folder, when syncing is on.
    pub fn sync_root(&self) -> Option<PathBuf> {
        if self.enabled { self.folder() } else { None }
    }

    /// Where earlier versions go: `Versions` in the cloud folder when syncing is on (so they are
    /// there on the other computer too), else in the settings folder. None when none are kept.
    pub fn versions_dir(&self) -> Option<PathBuf> {
        if self.keep_versions == 0 {
            return None;
        }
        self.sync_root().or_else(base_dir).map(|d| d.join("Versions"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_bad_files_give_the_defaults() {
        let d = crate::test_dir::fresh("settings");
        let f = d.join("sync.json");
        assert_eq!(Settings::load_from(&f), Settings::default());
        let s = Settings { enabled: true, folder: Some(PathBuf::from("D:\\iCloudDrive\\ArtCraft")), keep_versions: 7 };
        s.save_to(&f).unwrap();
        assert_eq!(Settings::load_from(&f), s);
        std::fs::write(&f, "{not json").unwrap();
        assert_eq!(Settings::load_from(&f), Settings::default());
        std::fs::write(&f, r#"{"enabled": "yes", "folder": "  ", "keepVersions": 99999999999}"#).unwrap();
        assert_eq!(Settings::load_from(&f), Settings { enabled: false, folder: None, keep_versions: MAX_VERSIONS });
    }

    #[test]
    fn versions_go_to_the_cloud_only_when_syncing() {
        let s = Settings { enabled: true, folder: Some(PathBuf::from("/cloud/ArtCraft")), keep_versions: 5 };
        assert_eq!(s.sync_root(), Some(PathBuf::from("/cloud/ArtCraft")));
        assert_eq!(s.versions_dir(), Some(PathBuf::from("/cloud/ArtCraft/Versions")));
        assert_eq!(Settings { keep_versions: 0, ..s.clone() }.versions_dir(), None);
        assert_eq!(Settings { enabled: false, ..s }.sync_root(), None);
    }
}
