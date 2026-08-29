use std::collections::HashSet;
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
        Self::at(&dirs.data_local_dir().join("mail.db"))
    }

    fn at(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS messages (
                 account TEXT NOT NULL,
                 id TEXT NOT NULL,
                 internal_date INTEGER NOT NULL DEFAULT 0,
                 message_json TEXT NOT NULL,
                 PRIMARY KEY(account, id)
             );",
        )?;
        Ok(Self { connection })
    }

    pub fn replace_inbox(&mut self, account: &str, messages: &[Message]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM messages WHERE account = ?1", [account])?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO messages(account, id, internal_date, message_json) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for message in messages {
                statement.execute(params![
                    account,
                    message.id,
                    message.internal_date.parse::<i64>().unwrap_or_default(),
                    serde_json::to_string(message)?,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn messages(&self, account: &str) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT message_json FROM messages WHERE account = ?1 ORDER BY internal_date DESC",
        )?;
        let values = statement
            .query_map([account], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        values
            .iter()
            .map(|value| serde_json::from_str(value).context("cached message is invalid"))
            .collect()
    }

    pub fn mark_read(&mut self, account: &str, message_ids: &[String]) -> Result<()> {
        let ids = message_ids.iter().collect::<HashSet<_>>();
        let mut messages = self.messages(account)?;
        for message in &mut messages {
            if ids.contains(&message.id) {
                message.label_ids.retain(|label| label != "UNREAD");
            }
        }
        self.replace_inbox(account, &messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replaces_and_reads_account_cache() {
        let mut cache = MailCache::at(Path::new(":memory:")).unwrap();
        let message: Message = serde_json::from_value(json!({
            "id": "one", "threadId": "thread", "internalDate": "42",
            "labelIds": ["INBOX", "UNREAD"], "snippet": "preview", "payload": {}
        }))
        .unwrap();
        cache
            .replace_inbox("person@example.com", &[message])
            .unwrap();
        assert_eq!(cache.messages("person@example.com").unwrap()[0].id, "one");
        cache
            .mark_read("person@example.com", &["one".to_owned()])
            .unwrap();
        assert_eq!(
            cache.messages("person@example.com").unwrap()[0].label_ids,
            vec!["INBOX"]
        );
    }
}
