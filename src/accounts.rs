use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::cache::MailCache;

const KEYRING_SERVICE: &str = "io.github.postbird.Mail";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Account {
    pub email: String,
    pub display_name: String,
    pub kind: AccountKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goa_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    GmailAppPassword,
    GnomeOnlineAccounts,
}

#[derive(Clone)]
pub struct AccountStore {
    config_dir: PathBuf,
}

impl AccountStore {
    pub fn open() -> Result<Self> {
        let dirs = ProjectDirs::from("io.github", "Postbird", "Postbird")
            .context("could not determine the user configuration directory")?;
        let store = Self {
            config_dir: dirs.config_dir().to_path_buf(),
        };
        fs::create_dir_all(&store.config_dir)?;
        Ok(store)
    }

    #[cfg(test)]
    fn at(config_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&config_dir)?;
        Ok(Self { config_dir })
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let path = self.config_dir.join("accounts.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let stored: Vec<serde_json::Value> = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("could not parse {}", path.display()))?;
        stored
            .into_iter()
            .filter(|account| {
                !matches!(
                    account.get("kind").and_then(serde_json::Value::as_str),
                    None | Some("google_oauth")
                )
            })
            .map(serde_json::from_value)
            .collect::<serde_json::Result<Vec<_>>>()
            .with_context(|| format!("could not parse {}", path.display()))
    }

    pub fn save_app_password_account(&self, account: Account, password: &str) -> Result<()> {
        if self
            .accounts()?
            .iter()
            .any(|existing| existing.email == account.email && existing.kind != account.kind)
        {
            bail!("Remove the existing account before changing its sign-in method");
        }
        keyring::Entry::new(KEYRING_SERVICE, &account.email)?.set_password(password)?;
        self.upsert_account(account)
    }

    pub fn save_goa_account(&self, account: Account) -> Result<()> {
        if account.kind != AccountKind::GnomeOnlineAccounts || account.goa_id.is_none() {
            bail!("A GOA account ID is required");
        }
        if self
            .accounts()?
            .iter()
            .any(|existing| existing.email == account.email && existing.kind != account.kind)
        {
            bail!("Remove the existing account before changing its sign-in method");
        }
        self.upsert_account(account)
    }

    fn upsert_account(&self, account: Account) -> Result<()> {
        let mut accounts = self.accounts()?;
        if let Some(existing) = accounts.iter_mut().find(|item| item.email == account.email) {
            *existing = account.clone();
        } else {
            accounts.push(account.clone());
            accounts.sort_by_key(|item| item.email.to_lowercase());
        }
        self.write_json("accounts.json", &accounts)
    }

    pub fn remove_account(&self, email: &str) -> Result<()> {
        let mut accounts = self.accounts()?;
        let kind = accounts
            .iter()
            .find(|account| account.email == email)
            .map(|account| account.kind);
        accounts.retain(|account| account.email != email);
        MailCache::open()?.remove_account(email)?;
        if kind == Some(AccountKind::GmailAppPassword) {
            let entry = keyring::Entry::new(KEYRING_SERVICE, email)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) => return Err(error.into()),
            };
        }
        self.write_json("accounts.json", &accounts)
    }

    pub fn app_password(&self, email: &str) -> Result<String> {
        keyring::Entry::new(KEYRING_SERVICE, email)?
            .get_password()
            .context("could not load this account's app password")
    }

    pub fn account(&self, email: &str) -> Result<Account> {
        self.accounts()?
            .into_iter()
            .find(|account| account.email == email)
            .with_context(|| format!("account {email} is not in Postbird"))
    }

    fn write_json<T: Serialize + ?Sized>(&self, filename: &str, value: &T) -> Result<()> {
        let destination = self.config_dir.join(filename);
        let temporary = self.config_dir.join(format!(".{filename}.tmp"));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        fs::rename(temporary, destination)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_metadata_round_trips() {
        let directory = std::env::temp_dir().join(format!("postbird-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let store = AccountStore::at(directory.clone()).unwrap();
        store
            .write_json(
                "accounts.json",
                &[Account {
                    email: "person@example.com".into(),
                    display_name: "Person".into(),
                    kind: AccountKind::GmailAppPassword,
                    goa_id: None,
                }],
            )
            .unwrap();
        assert_eq!(store.accounts().unwrap()[0].email, "person@example.com");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn old_oauth_accounts_do_not_block_supported_accounts() {
        let directory = std::env::temp_dir().join(format!(
            "postbird-legacy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = AccountStore::at(directory.clone()).unwrap();
        store
            .write_json(
                "accounts.json",
                &serde_json::json!([
                    {"email":"old@example.com","display_name":"Old"},
                    {"email":"oauth@example.com","display_name":"OAuth","kind":"google_oauth"},
                    {"email":"goa@example.com","display_name":"GOA","kind":"gnome_online_accounts","goa_id":"goa-1"},
                    {"email":"app@example.com","display_name":"App","kind":"gmail_app_password"}
                ]),
            )
            .unwrap();
        let accounts = store.accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(
            accounts
                .iter()
                .any(|account| account.kind == AccountKind::GnomeOnlineAccounts)
        );
        assert!(
            accounts
                .iter()
                .any(|account| account.kind == AccountKind::GmailAppPassword)
        );
        assert!(
            std::fs::read_to_string(directory.join("accounts.json"))
                .unwrap()
                .contains("oauth@example.com")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn goa_account_saves_only_its_account_id() {
        let directory = std::env::temp_dir().join(format!(
            "postbird-goa-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = AccountStore::at(directory.clone()).unwrap();
        store
            .save_goa_account(Account {
                email: "person@example.com".into(),
                display_name: "person@example.com (GOA)".into(),
                kind: AccountKind::GnomeOnlineAccounts,
                goa_id: Some("goa-account-123".into()),
            })
            .unwrap();
        let saved = std::fs::read_to_string(directory.join("accounts.json")).unwrap();
        assert!(saved.contains("goa-account-123"));
        assert!(!saved.contains("access_token"));
        assert_eq!(
            store.accounts().unwrap()[0].goa_id.as_deref(),
            Some("goa-account-123")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
