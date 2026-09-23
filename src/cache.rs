use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::{Connection, TransactionBehavior, params};

const CACHE_SCHEMA_VERSION: i64 = 1;
static CACHE_SCHEMA_LOCK: Mutex<()> = Mutex::new(());

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
        // Startup launches several cache users together. Serialize setup inside
        // this process, and let SQLite arbitrate with any other process.
        let _schema_guard = CACHE_SCHEMA_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("cache initialization lock was poisoned"))?;
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= CACHE_SCHEMA_VERSION {
            return Ok(Self { connection });
        }
        connection.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Deferred read-then-write transactions can fail immediately with
        // SQLITE_BUSY_SNAPSHOT even when a busy timeout is configured.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS mailbox_messages (
                 account TEXT NOT NULL,
                 mailbox TEXT NOT NULL,
                 id TEXT NOT NULL,
                 internal_date INTEGER NOT NULL DEFAULT 0,
                 message_json TEXT NOT NULL,
                 PRIMARY KEY(account, mailbox, id)
             );
             CREATE TABLE IF NOT EXISTS message_images (
                 account TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 part_id TEXT NOT NULL,
                 data BLOB NOT NULL,
                 PRIMARY KEY(account, message_id, part_id)
             );
             CREATE TABLE IF NOT EXISTS imap_backoff (
                 account TEXT PRIMARY KEY,
                 retry_at INTEGER NOT NULL
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
        transaction.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(Self { connection })
    }

    pub fn replace_mailbox(
        &mut self,
        account: &str,
        mailbox: &str,
        messages: &[Message],
    ) -> Result<()> {
        // Encoding embedded images can be expensive; do it before taking the
        // write lock so other accounts can continue using the cache.
        let serialized = messages
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM mailbox_messages WHERE account = ?1 AND mailbox = ?2",
            params![account, mailbox],
        )?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO mailbox_messages(account, mailbox, id, internal_date, message_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (message, json) in messages.iter().zip(serialized) {
                statement.execute(params![
                    account,
                    mailbox,
                    message.id,
                    message.internal_date.parse::<i64>().unwrap_or_default(),
                    json,
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
        self.set_label(account, message_ids, "UNREAD", false)
    }

    pub fn inline_images(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let mut statement = self.connection.prepare(
            "SELECT part_id, data FROM message_images WHERE account = ?1 AND message_id = ?2",
        )?;
        Ok(statement
            .query_map(params![account, message_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn store_inline_images(
        &mut self,
        account: &str,
        message_id: &str,
        images: &HashMap<String, Vec<u8>>,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (part_id, data) in images {
            transaction.execute(
                "INSERT OR REPLACE INTO message_images(account, message_id, part_id, data)
                 VALUES (?1, ?2, ?3, ?4)",
                params![account, message_id, part_id, data],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn set_label(
        &mut self,
        account: &str,
        message_ids: &[String],
        label: &str,
        enabled: bool,
    ) -> Result<()> {
        let ids = message_ids.iter().collect::<HashSet<_>>();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT mailbox, id, message_json FROM mailbox_messages WHERE account = ?1",
            )?;
            statement
                .query_map([account], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        {
            let mut update = transaction.prepare(
                "UPDATE mailbox_messages SET message_json = ?4 WHERE account = ?1 AND mailbox = ?2 AND id = ?3",
            )?;
            let mut delete = transaction.prepare(
                "DELETE FROM mailbox_messages WHERE account = ?1 AND mailbox = ?2 AND id = ?3",
            )?;
            for (mailbox, id, value) in rows {
                if !ids.contains(&id) {
                    continue;
                }
                if mailbox == "STARRED" && label == "STARRED" && !enabled {
                    delete.execute(params![account, mailbox, id])?;
                    continue;
                }
                let mut message: Message =
                    serde_json::from_str(&value).context("cached message is invalid")?;
                if enabled {
                    if !message.label_ids.iter().any(|existing| existing == label) {
                        message.label_ids.push(label.to_owned());
                    }
                } else {
                    message.label_ids.retain(|existing| existing != label);
                }
                update.execute(params![
                    account,
                    mailbox,
                    id,
                    serde_json::to_string(&message)?
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_messages(&mut self, account: &str, message_ids: &[String]) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut delete = transaction.prepare(
                "DELETE FROM mailbox_messages WHERE account = ?1 AND mailbox = 'INBOX' AND id = ?2",
            )?;
            for id in message_ids {
                delete.execute(params![account, id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_messages_everywhere(
        &mut self,
        account: &str,
        message_ids: &[String],
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut delete = transaction
                .prepare("DELETE FROM mailbox_messages WHERE account = ?1 AND id = ?2")?;
            for id in message_ids {
                delete.execute(params![account, id])?;
                transaction.execute(
                    "DELETE FROM message_images WHERE account = ?1 AND message_id = ?2",
                    params![account, id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn imap_retry_at(&self, account: &str, now: i64) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .connection
            .query_row(
                "SELECT retry_at FROM imap_backoff WHERE account = ?1 AND retry_at > ?2",
                params![account, now],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn pause_imap(&self, account: &str, retry_at: i64) -> Result<()> {
        self.connection.execute(
            "INSERT INTO imap_backoff(account, retry_at) VALUES (?1, ?2)
             ON CONFLICT(account) DO UPDATE SET retry_at = MAX(retry_at, excluded.retry_at)",
            params![account, retry_at],
        )?;
        Ok(())
    }

    pub fn remove_account(&mut self, account: &str) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM mailbox_messages WHERE account = ?1", [account])?;
        transaction.execute("DELETE FROM message_images WHERE account = ?1", [account])?;
        transaction.execute("DELETE FROM imap_backoff WHERE account = ?1", [account])?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opening_an_initialized_cache_does_not_need_the_write_lock() {
        let directory = std::env::temp_dir().join(format!(
            "postbird-cache-reader-{}",
            gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("mail.db");
        let mut writer = MailCache::at(&path).unwrap();
        let transaction = writer
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction
            .execute(
                "INSERT INTO imap_backoff(account, retry_at) VALUES ('account', 1000)",
                [],
            )
            .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let read_result = std::thread::scope(|scope| {
            scope.spawn(|| {
                let result =
                    MailCache::at(&path).and_then(|cache| cache.imap_retry_at("account", 999));
                sender.send(result).unwrap();
            });
            // The reader must finish while the write is still uncommitted.
            let result = receiver.recv_timeout(Duration::from_secs(2));
            transaction.commit().unwrap();
            result
        });
        assert_eq!(read_result.unwrap().unwrap(), None);
        assert_eq!(writer.imap_retry_at("account", 999).unwrap(), Some(1000));
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_startup_and_cache_updates_do_not_lock_each_other_out() {
        let directory = std::env::temp_dir().join(format!(
            "postbird-concurrent-{}",
            gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("mail.db");
        let barrier = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let workers = (0..8)
                .map(|index| {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || -> Result<()> {
                        barrier.wait();
                        let account = format!("account-{index}@example.com");
                        for _ in 0..12 {
                            let mut cache = MailCache::at(path)?;
                            cache.replace_mailbox(&account, "INBOX", &[test_message()])?;
                            cache.mark_read(&account, &["one".into()])?;
                            cache.set_label(&account, &["one".into()], "STARRED", true)?;
                            cache.pause_imap(&account, 1000)?;
                            assert_eq!(cache.imap_retry_at(&account, 999)?, Some(1000));
                            let messages = cache.messages(&account, "INBOX")?;
                            assert_eq!(messages.len(), 1);
                            assert!(!messages[0].label_ids.contains(&"UNREAD".into()));
                            assert!(messages[0].label_ids.contains(&"STARRED".into()));
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        std::fs::remove_dir_all(directory).unwrap();
        for result in results {
            result.unwrap();
        }
    }

    #[test]
    fn imap_pause_survives_reopening_and_only_blocks_the_limited_account() {
        let path = std::env::temp_dir().join(format!(
            "postbird-backoff-{}.db",
            gtk::glib::uuid_string_random()
        ));
        {
            let cache = MailCache::at(&path).unwrap();
            cache.pause_imap("limited@example.com", 1000).unwrap();
            cache.pause_imap("limited@example.com", 900).unwrap();
        }
        {
            let mut cache = MailCache::at(&path).unwrap();
            assert_eq!(
                cache.imap_retry_at("limited@example.com", 999).unwrap(),
                Some(1000)
            );
            assert_eq!(cache.imap_retry_at("other@example.com", 999).unwrap(), None);
            assert_eq!(
                cache.imap_retry_at("limited@example.com", 1000).unwrap(),
                None
            );
            cache.remove_account("limited@example.com").unwrap();
            assert_eq!(
                cache.imap_retry_at("limited@example.com", 999).unwrap(),
                None
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    fn test_message() -> Message {
        serde_json::from_value(json!({
            "id": "one", "threadId": "thread", "internalDate": "42",
            "labelIds": ["INBOX", "UNREAD"], "snippet": "preview", "payload": {}
        }))
        .unwrap()
    }

    #[test]
    fn body_and_embedded_images_survive_cache_reopening() {
        let path = std::env::temp_dir().join(format!(
            "postbird-image-cache-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let message: Message = serde_json::from_value(json!({
            "id": "one", "threadId": "thread", "payload": {
                "mimeType": "multipart/related", "parts": [
                    {"mimeType": "text/html", "body": {"data": "PGI-SGVsbG88L2I-"}},
                    {"mimeType": "image/png", "headers": [{"name": "Content-ID", "value": "<logo>"}],
                     "body": {"data": "AQID"}}
                ]
            }
        })).unwrap();
        let images = HashMap::from([("1".to_owned(), vec![1, 2, 3])]);
        {
            let mut cache = MailCache::at(&path).unwrap();
            cache
                .replace_mailbox("one@example.com", "INBOX", &[message])
                .unwrap();
            cache
                .store_inline_images("one@example.com", "older", &images)
                .unwrap();
            cache
                .store_inline_images("two@example.com", "older", &images)
                .unwrap();
        }
        let mut cache = MailCache::at(&path).unwrap();
        let message = &cache.messages("one@example.com", "INBOX").unwrap()[0];
        assert_eq!(message.body_html().as_deref(), Some("<b>Hello</b>"));
        assert_eq!(
            message.inline_image_parts()[0].0.body.data.as_deref(),
            Some("AQID")
        );
        assert_eq!(
            cache.inline_images("one@example.com", "older").unwrap(),
            images
        );
        assert!(
            cache
                .inline_images("missing@example.com", "older")
                .unwrap()
                .is_empty()
        );
        cache
            .replace_mailbox("one@example.com", "INBOX", &[])
            .unwrap();
        assert_eq!(
            cache.inline_images("one@example.com", "older").unwrap(),
            images
        );
        cache.remove_account("one@example.com").unwrap();
        assert!(
            cache
                .inline_images("one@example.com", "older")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cache.inline_images("two@example.com", "older").unwrap(),
            images
        );
        cache
            .remove_messages_everywhere("two@example.com", &["older".to_owned()])
            .unwrap();
        assert!(
            cache
                .inline_images("two@example.com", "older")
                .unwrap()
                .is_empty()
        );
        drop(cache);
        std::fs::remove_file(path).unwrap();
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
        assert_eq!(
            cache.messages("person@example.com", "SENT").unwrap()[0].label_ids,
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
    fn removing_account_clears_all_its_mailboxes_only() {
        let mut cache = MailCache::at(Path::new(":memory:")).unwrap();
        let message = test_message();
        for account in ["deleted@example.com", "kept@example.com"] {
            for mailbox in ["INBOX", "SENT"] {
                cache
                    .replace_mailbox(account, mailbox, std::slice::from_ref(&message))
                    .unwrap();
            }
        }

        cache.remove_account("deleted@example.com").unwrap();

        for mailbox in ["INBOX", "SENT"] {
            assert!(
                cache
                    .messages("deleted@example.com", mailbox)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                cache.messages("kept@example.com", mailbox).unwrap().len(),
                1
            );
        }
    }

    #[test]
    fn local_actions_update_each_cached_copy_without_touching_other_accounts() {
        let mut cache = MailCache::at(Path::new(":memory:")).unwrap();
        let message = test_message();
        for account in ["acted@example.com", "kept@example.com"] {
            for mailbox in ["INBOX", "STARRED"] {
                cache
                    .replace_mailbox(account, mailbox, std::slice::from_ref(&message))
                    .unwrap();
            }
        }
        cache
            .set_label("acted@example.com", &["one".into()], "STARRED", true)
            .unwrap();
        for mailbox in ["INBOX", "STARRED"] {
            assert!(
                cache.messages("acted@example.com", mailbox).unwrap()[0]
                    .label_ids
                    .contains(&"STARRED".to_owned())
            );
            assert!(
                !cache.messages("kept@example.com", mailbox).unwrap()[0]
                    .label_ids
                    .contains(&"STARRED".to_owned())
            );
        }
        cache
            .set_label("acted@example.com", &["one".into()], "STARRED", false)
            .unwrap();
        assert!(
            !cache.messages("acted@example.com", "INBOX").unwrap()[0]
                .label_ids
                .contains(&"STARRED".to_owned())
        );
        assert!(
            cache
                .messages("acted@example.com", "STARRED")
                .unwrap()
                .is_empty()
        );
        cache
            .remove_messages_everywhere("acted@example.com", &["one".into()])
            .unwrap();
        for mailbox in ["INBOX", "STARRED"] {
            assert!(
                cache
                    .messages("acted@example.com", mailbox)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                cache.messages("kept@example.com", mailbox).unwrap().len(),
                1
            );
        }
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
