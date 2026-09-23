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
    pub notify_new_mail: bool,
    pub play_new_mail_sound: bool,
    pub favorite_folders: Vec<FavoriteFolder>,
    pub favorite_order: Vec<FavoriteFolder>,
    pub unfavorite_inboxes: Vec<String>,
    pub favorite_names: Vec<FavoriteName>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FavoriteFolder {
    pub account_email: String,
    pub label_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FavoriteName {
    pub account_email: String,
    pub label_id: String,
    pub name: String,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            mailbox_split: 220,
            message_split: 390,
            load_remote_images: false,
            notify_new_mail: true,
            play_new_mail_sound: false,
            favorite_folders: Vec::new(),
            favorite_order: Vec::new(),
            unfavorite_inboxes: Vec::new(),
            favorite_names: Vec::new(),
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

    pub fn is_favorite(&self, account_email: &str, label_id: &str) -> bool {
        if label_id == "INBOX" {
            !self
                .unfavorite_inboxes
                .iter()
                .any(|email| email == account_email)
        } else {
            self.favorite_folders
                .iter()
                .any(|folder| folder.account_email == account_email && folder.label_id == label_id)
        }
    }

    pub fn toggle_favorite(&mut self, account_email: &str, label_id: &str) {
        if label_id == "INBOX" {
            if self.is_favorite(account_email, label_id) {
                self.unfavorite_inboxes.push(account_email.to_owned());
                self.favorite_order.retain(|folder| {
                    folder.account_email != account_email || folder.label_id != label_id
                });
            } else {
                self.unfavorite_inboxes
                    .retain(|email| email != account_email);
            }
            return;
        }
        if self.is_favorite(account_email, label_id) {
            self.favorite_folders.retain(|folder| {
                folder.account_email != account_email || folder.label_id != label_id
            });
            self.favorite_order.retain(|folder| {
                folder.account_email != account_email || folder.label_id != label_id
            });
        } else {
            self.favorite_folders.push(FavoriteFolder {
                account_email: account_email.to_owned(),
                label_id: label_id.to_owned(),
            });
        }
    }

    pub fn ordered_favorites(&self, available: &[FavoriteFolder]) -> Vec<FavoriteFolder> {
        let mut ordered = Vec::with_capacity(available.len());
        for folder in self.favorite_order.iter().chain(available) {
            if available.contains(folder) && !ordered.contains(folder) {
                ordered.push(folder.clone());
            }
        }
        ordered
    }

    pub fn move_favorite(
        &mut self,
        available: &[FavoriteFolder],
        source: &FavoriteFolder,
        target: &FavoriteFolder,
        after: bool,
    ) -> bool {
        if source == target || !available.contains(source) || !available.contains(target) {
            return false;
        }
        let mut ordered = self.ordered_favorites(available);
        let previous = ordered.clone();
        let source_index = ordered.iter().position(|folder| folder == source).unwrap();
        let moved = ordered.remove(source_index);
        let target_index = ordered.iter().position(|folder| folder == target).unwrap();
        ordered.insert(target_index + usize::from(after), moved);
        if ordered == previous {
            return false;
        }
        let mut reordered = ordered.into_iter();
        let mut saved_order = Vec::new();
        for folder in &self.favorite_order {
            if available.contains(folder) {
                if let Some(next) = reordered.next() {
                    saved_order.push(next);
                }
            } else {
                saved_order.push(folder.clone());
            }
        }
        saved_order.extend(reordered);
        self.favorite_order = saved_order;
        true
    }

    pub fn favorite_name(&self, account_email: &str, label_id: &str) -> Option<&str> {
        self.favorite_names
            .iter()
            .find(|folder| folder.account_email == account_email && folder.label_id == label_id)
            .map(|folder| folder.name.as_str())
    }

    pub fn set_favorite_name(&mut self, account_email: &str, label_id: &str, name: &str) {
        self.favorite_names
            .retain(|folder| folder.account_email != account_email || folder.label_id != label_id);
        let name = name.trim();
        if !name.is_empty() {
            self.favorite_names.push(FavoriteName {
                account_email: account_email.to_owned(),
                label_id: label_id.to_owned(),
                name: name.to_owned(),
            });
        }
    }

    pub fn remove_account(&mut self, account_email: &str) {
        self.favorite_folders
            .retain(|folder| folder.account_email != account_email);
        self.favorite_order
            .retain(|folder| folder.account_email != account_email);
        self.unfavorite_inboxes
            .retain(|email| email != account_email);
        self.favorite_names
            .retain(|folder| folder.account_email != account_email);
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
        assert!(preferences.notify_new_mail);
        assert!(!preferences.play_new_mail_sound);
        assert!(preferences.is_favorite("reader@example.com", "INBOX"));
        assert!(preferences.favorite_order.is_empty());
    }

    #[test]
    fn notification_options_persist_independently() {
        let preferences = UiPreferences {
            notify_new_mail: false,
            play_new_mail_sound: true,
            ..UiPreferences::default()
        };
        let loaded: UiPreferences =
            serde_json::from_str(&serde_json::to_string(&preferences).unwrap()).unwrap();
        assert!(!loaded.notify_new_mail);
        assert!(loaded.play_new_mail_sound);
    }

    #[test]
    fn favorites_default_to_inbox_and_can_be_changed_per_account() {
        let mut preferences = UiPreferences::default();
        preferences.toggle_favorite("one@example.com", "INBOX");
        preferences.toggle_favorite("one@example.com", "Projects");
        assert!(!preferences.is_favorite("one@example.com", "INBOX"));
        assert!(preferences.is_favorite("two@example.com", "INBOX"));
        assert!(preferences.is_favorite("one@example.com", "Projects"));
        preferences.set_favorite_name("one@example.com", "Projects", "Work projects");
        preferences.set_favorite_name("two@example.com", "INBOX", "Personal");
        let persisted = serde_json::to_string(&preferences).unwrap();
        let mut loaded: UiPreferences = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            loaded.favorite_name("one@example.com", "Projects"),
            Some("Work projects")
        );
        assert_eq!(
            loaded.favorite_name("two@example.com", "INBOX"),
            Some("Personal")
        );
        loaded.set_favorite_name("two@example.com", "INBOX", "  ");
        assert_eq!(loaded.favorite_name("two@example.com", "INBOX"), None);
        loaded.remove_account("one@example.com");
        assert!(loaded.is_favorite("one@example.com", "INBOX"));
        assert!(!loaded.is_favorite("one@example.com", "Projects"));
        assert_eq!(loaded.favorite_name("one@example.com", "Projects"), None);
    }

    #[test]
    fn favorite_order_moves_across_accounts_and_survives_reload() {
        let first = FavoriteFolder {
            account_email: "one@example.com".into(),
            label_id: "INBOX".into(),
        };
        let second = FavoriteFolder {
            account_email: "two@example.com".into(),
            label_id: "INBOX".into(),
        };
        let custom = FavoriteFolder {
            account_email: "one@example.com".into(),
            label_id: "Projects".into(),
        };
        let available = vec![first.clone(), second.clone(), custom.clone()];
        let mut preferences = UiPreferences::default();
        preferences.toggle_favorite("one@example.com", "Projects");
        assert_eq!(preferences.ordered_favorites(&available), available);
        assert!(preferences.move_favorite(&available, &custom, &first, false));
        assert_eq!(
            preferences.ordered_favorites(&available),
            [custom.clone(), first.clone(), second.clone()]
        );
        assert!(!preferences.move_favorite(&available, &custom, &first, false));
        let reloaded: UiPreferences =
            serde_json::from_str(&serde_json::to_string(&preferences).unwrap()).unwrap();
        assert_eq!(
            reloaded.ordered_favorites(&available),
            [custom.clone(), first.clone(), second.clone()]
        );
        let new_inbox = FavoriteFolder {
            account_email: "three@example.com".into(),
            label_id: "INBOX".into(),
        };
        assert_eq!(
            reloaded.ordered_favorites(&[available, vec![new_inbox.clone()]].concat()),
            [custom, first, second, new_inbox]
        );
    }

    #[test]
    fn removing_favorite_removes_its_saved_position() {
        let first = FavoriteFolder {
            account_email: "one@example.com".into(),
            label_id: "INBOX".into(),
        };
        let second = FavoriteFolder {
            account_email: "two@example.com".into(),
            label_id: "INBOX".into(),
        };
        let mut preferences = UiPreferences::default();
        let available = [first.clone(), second.clone()];
        assert!(preferences.move_favorite(&available, &second, &first, false));
        preferences.toggle_favorite("two@example.com", "INBOX");
        assert_eq!(preferences.favorite_order, std::slice::from_ref(&first));
        preferences.toggle_favorite("two@example.com", "INBOX");
        assert_eq!(preferences.ordered_favorites(&available), [first, second]);
    }

    #[test]
    fn dragging_before_a_custom_folder_loads_keeps_its_saved_place() {
        let inbox = FavoriteFolder {
            account_email: "one@example.com".into(),
            label_id: "INBOX".into(),
        };
        let other = FavoriteFolder {
            account_email: "two@example.com".into(),
            label_id: "INBOX".into(),
        };
        let delayed = FavoriteFolder {
            account_email: "one@example.com".into(),
            label_id: "Projects".into(),
        };
        let mut preferences = UiPreferences {
            favorite_order: vec![inbox.clone(), delayed.clone(), other.clone()],
            ..UiPreferences::default()
        };
        assert!(preferences.move_favorite(&[inbox.clone(), other.clone()], &other, &inbox, false,));
        assert_eq!(
            preferences.ordered_favorites(&[inbox.clone(), other.clone(), delayed.clone()]),
            [other, delayed, inbox]
        );
    }
}
