use std::collections::HashSet;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::{Connection, params};

use crate::gmail::Message;

pub struct MailCache {
    connection: Connection,
}

impl MailCache {
    pub fn open() -> Result<Self> {
        let dirs = ProjectDirs::from("io.github", "Postbird", "Postbird")
            .context("could not determine the user data directory")?;
        std::fs::create_dir_all(dirs.data_local_dir())?;
        std::fs::set_permissions(
            dirs.data_local_dir(),
            std::fs::Permissions::from_mode(0o700),
        )?;
        let path = dirs.data_local_dir().join("mail.db");
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Self::at(&path)
    }

    fn at(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode=WAL;")?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS mailbox_messages (
                 account TEXT NOT NULL,
                 mailbox TEXT NOT NULL,
                 id TEXT NOT NULL,
                 internal_date INTEGER NOT NULL DEFAULT 0,
                 message_json TEXT NOT NULL,
                 PRIMARY KEY(account, mailbox, id)
             );",
        )?;
        let has_legacy_inbox = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_legacy_inbox {
            transaction.execute(
                "INSERT OR IGNORE INTO mailbox_messages(account, mailbox, id, internal_date, message_json)
                 SELECT account, 'INBOX', id, internal_date, message_json FROM messages",
                [],
            )?;
            transaction.execute("DROP TABLE messages", [])?;
        }
        transaction.commit()?;
        Ok(Self { connection })
    }

    pub fn replace_mailbox(
        &mut self,
        account: &str,
        mailbox: &str,
        messages: &[Message],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM mailbox_messages WHERE account = ?1 AND mailbox = ?2",
            params![account, mailbox],
        )?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO mailbox_messages(account, mailbox, id, internal_date, message_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for message in messages {
                statement.execute(params![
                    account,
                    mailbox,
                    message.id,
                    message.internal_date.parse::<i64>().unwrap_or_default(),
                    serde_json::to_string(message)?,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn messages(&self, account: &str, mailbox: &str) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT message_json FROM mailbox_messages
             WHERE account = ?1 AND mailbox = ?2 ORDER BY internal_date DESC",
        )?;
        let values = statement
            .query_map(params![account, mailbox], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        values
            .iter()
            .map(|value| serde_json::from_str(value).context("cached message is invalid"))
            .collect()
    }

    pub fn mark_read(&mut self, account: &str, message_ids: &[String]) -> Result<()> {
        let ids = message_ids.iter().collect::<HashSet<_>>();
        let mut messages = self.messages(account, "INBOX")?;
        for message in &mut messages {
            if ids.contains(&message.id) {
                message.label_ids.retain(|label| label != "UNREAD");
            }
        }
        self.replace_mailbox(account, "INBOX", &messages)
    }

    pub fn remove_messages(&mut self, account: &str, message_ids: &[String]) -> Result<()> {
        let ids = message_ids.iter().collect::<HashSet<_>>();
        let messages = self
            .messages(account, "INBOX")?
            .into_iter()
            .filter(|message| !ids.contains(&message.id))
            .collect::<Vec<_>>();
        self.replace_mailbox(account, "INBOX", &messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_message() -> Message {
        serde_json::from_value(json!({
            "id": "one", "threadId": "thread", "internalDate": "42",
            "labelIds": ["INBOX", "UNREAD"], "snippet": "preview", "payload": {}
        }))
        .unwrap()
    }

    #[test]
    fn replaces_and_reads_account_cache() {
        let mut cache = MailCache::at(Path::new(":memory:")).unwrap();
        let message = test_message();
        cache
            .replace_mailbox(
                "person@example.com",
                "INBOX",
                std::slice::from_ref(&message),
            )
            .unwrap();
        cache
            .replace_mailbox("person@example.com", "SENT", std::slice::from_ref(&message))
            .unwrap();
        assert_eq!(
            cache.messages("person@example.com", "INBOX").unwrap()[0].id,
            "one"
        );
        assert_eq!(
            cache.messages("person@example.com", "SENT").unwrap()[0].id,
            "one"
        );
        cache
            .mark_read("person@example.com", &["one".to_owned()])
            .unwrap();
        assert_eq!(
            cache.messages("person@example.com", "INBOX").unwrap()[0].label_ids,
            vec!["INBOX"]
        );
        cache
            .remove_messages("person@example.com", &["one".to_owned()])
            .unwrap();
        assert!(
            cache
                .messages("person@example.com", "INBOX")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cache.messages("person@example.com", "SENT").unwrap().len(),
            1
        );
    }

    #[test]
    fn migrates_the_existing_inbox_cache() {
        let directory =
            std::env::temp_dir().join(format!("postbird-cache-migration-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("mail.db");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE messages (
                    account TEXT NOT NULL,
                    id TEXT NOT NULL,
                    internal_date INTEGER NOT NULL DEFAULT 0,
                    message_json TEXT NOT NULL,
                    PRIMARY KEY(account, id)
                );",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO messages(account, id, internal_date, message_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    "person@example.com",
                    "one",
                    42,
                    serde_json::to_string(&test_message()).unwrap()
                ],
            )
            .unwrap();
        drop(legacy);

        let cache = MailCache::at(&path).unwrap();
        assert_eq!(
            cache.messages("person@example.com", "INBOX").unwrap()[0].id,
            "one"
        );
        assert!(
            cache
                .messages("person@example.com", "SENT")
                .unwrap()
                .is_empty()
        );
        drop(cache);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
