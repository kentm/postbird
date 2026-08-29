use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::oauth::{OAuthCredentials, TokenSet};

const KEYRING_SERVICE: &str = "io.github.postbird.Mail";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Account {
    pub email: String,
    pub display_name: String,
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
        serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("could not parse {}", path.display()))
    }

    pub fn save_account(&self, account: Account, token: &TokenSet) -> Result<()> {
        let mut accounts = self.accounts()?;
        if let Some(existing) = accounts.iter_mut().find(|item| item.email == account.email) {
            *existing = account.clone();
        } else {
            accounts.push(account.clone());
            accounts.sort_by_key(|item| item.email.to_lowercase());
        }
        self.write_json("accounts.json", &accounts)?;
        self.save_token(&account.email, token)
    }

    pub fn remove_account(&self, email: &str) -> Result<()> {
        let mut accounts = self.accounts()?;
        accounts.retain(|account| account.email != email);
        self.write_json("accounts.json", &accounts)?;
        let entry = keyring::Entry::new(KEYRING_SERVICE, email)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save_token(&self, email: &str, token: &TokenSet) -> Result<()> {
        keyring::Entry::new(KEYRING_SERVICE, email)?
            .set_password(&serde_json::to_string(token)?)?;
        Ok(())
    }

    pub fn token(&self, email: &str) -> Result<TokenSet> {
        let value = keyring::Entry::new(KEYRING_SERVICE, email)?.get_password()?;
        serde_json::from_str(&value).context("the stored Google token is invalid")
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.config_dir.join("google-oauth.json")
    }

    pub fn credentials(&self) -> Result<OAuthCredentials> {
        OAuthCredentials::from_google_file(&self.credentials_path())
    }

    pub fn import_credentials(&self, source: &std::path::Path) -> Result<()> {
        OAuthCredentials::from_google_file(source)?;
        let contents = fs::read(source)?;
        let mut destination = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(self.credentials_path())?;
        destination.write_all(&contents)?;
        Ok(())
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
                }],
            )
            .unwrap();
        assert_eq!(store.accounts().unwrap()[0].email, "person@example.com");
        fs::remove_dir_all(directory).unwrap();
    }
}
