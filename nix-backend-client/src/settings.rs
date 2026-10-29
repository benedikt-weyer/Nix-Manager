//! Settings a user types into a frontend and expects to find again next time.
//!
//! Stored as a small JSON file under the user's config directory so every
//! frontend can share the same one.

use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

/// Empty strings mean "use the backend's default".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub config_dir: String,
    #[serde(default)]
    pub host_name: String,
}

impl Settings {
    /// `$XDG_CONFIG_HOME/nix-manager/settings.json`, falling back to
    /// `~/.config/...`. `None` if neither variable is set.
    pub fn path() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".config"),
        };
        Some(base.join("nix-manager").join("settings.json"))
    }

    /// Missing or unreadable settings are not an error: you just start fresh.
    pub fn load() -> Self {
        Self::path().map(|path| Self::load_from(&path)).unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path().ok_or_else(|| io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?;
        self.save_to(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    /// Writes to a temp file and renames, so a crash never leaves half a file.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_string_pretty(self).map_err(io::Error::other)?)?;
        fs::rename(&temp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nix-manager-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn round_trips() {
        let dir = scratch("roundtrip");
        let path = dir.join("nested").join("settings.json");
        let settings = Settings { config_dir: "/home/me/nixos".to_owned(), host_name: "laptop".to_owned() };

        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_corrupt_file_gives_defaults() {
        let dir = scratch("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        assert_eq!(Settings::load_from(&path), Settings::default());
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        fs::write(&path, r#"{"host_name":"only-host"}"#).unwrap();
        assert_eq!(Settings::load_from(&path).host_name, "only-host");

        let _ = fs::remove_dir_all(dir);
    }
}
