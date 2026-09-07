use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UiPreferences {
    pub mailbox_split: i32,
    pub message_split: i32,
    pub load_remote_images: bool,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            mailbox_split: 220,
            message_split: 390,
            load_remote_images: false,
        }
    }
}

impl UiPreferences {
    fn path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("io.github", "Postbird", "Postbird")
            .context("could not determine the user configuration directory")?;
        fs::create_dir_all(dirs.config_dir())?;
        Ok(dirs.config_dir().join("ui.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|path| Ok(serde_json::from_slice(&fs::read(path)?)?))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_missing_fields_for_older_preferences() {
        let preferences: UiPreferences = serde_json::from_str(r#"{"mailbox_split":300}"#).unwrap();
        assert_eq!(preferences.mailbox_split, 300);
        assert_eq!(preferences.message_split, 390);
        assert!(!preferences.load_remote_images);
    }
}
