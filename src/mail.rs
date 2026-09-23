use std::{
    collections::HashMap,
    io::Write,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    accounts::{AccountKind, AccountStore},
    cache::MailCache,
    gmail::{Body, ComposeMessage, Draft, Label, Thread, ThreadPage, ThreadRef, encode_message},
};

const HELPER: &str = include_str!("../scripts/gmail_imap_backend.py");

pub struct ImapClient {
    email: String,
    store: AccountStore,
    auth: ImapAuth,
    cancelled: Option<Arc<AtomicBool>>,
}

pub type MailClient = ImapClient;

enum ImapAuth {
    AppPassword,
    Goa(String),
}

#[derive(Clone, Deserialize)]
pub struct GoaAccount {
    pub id: String,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MailCursor {
    pub uidvalidity: u64,
    pub uidnext: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IncomingMail {
    pub id: String,
    pub thread_id: String,
    pub sender: String,
    pub subject: String,
}

#[derive(Deserialize)]
pub struct IncomingMailPoll {
    pub cursor: MailCursor,
    pub messages: Vec<IncomingMail>,
}

#[derive(Deserialize)]
pub struct BatchUpdateResult {
    pub updated: Vec<String>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct HelperResponse {
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
    error_kind: Option<String>,
}

fn imap_pause_message(email: &str, retry_at: i64) -> String {
    let until = chrono::DateTime::from_timestamp(retry_at, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string();
    format!(
        "Gmail is limiting IMAP access for {email}. Requests are paused until {until}; cached mail remains available."
    )
}

impl ImapClient {
    pub fn app_password(email: String, store: AccountStore) -> Self {
        Self {
            email,
            store,
            auth: ImapAuth::AppPassword,
            cancelled: None,
        }
    }

    pub fn goa(email: String, id: String, store: AccountStore) -> Self {
        Self {
            email,
            store,
            auth: ImapAuth::Goa(id),
            cancelled: None,
        }
    }

    fn call<T: DeserializeOwned>(&self, operation: &str, fields: Value) -> Result<T> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            bail!("Mail request cancelled");
        }
        let credential = match &self.auth {
            ImapAuth::AppPassword => {
                json!({"password": self.store.app_password(&self.email)?})
            }
            ImapAuth::Goa(id) => json!({"goa_id": id}),
        };
        Self::invoke(&self.email, credential, operation, fields)
    }

    fn invoke<T: DeserializeOwned>(
        email: &str,
        credential: Value,
        operation: &str,
        fields: Value,
    ) -> Result<T> {
        // Persist the cooldown so closing/reopening Postbird doesn't hammer an
        // account that Gmail has already limited. SMTP-only sends are separate.
        let cache = (!email.is_empty() && operation != "send")
            .then(MailCache::open)
            .transpose()?;
        if let Some(cache) = &cache
            && let Some(retry_at) = cache.imap_retry_at(email, chrono::Utc::now().timestamp())?
        {
            bail!("{}", imap_pause_message(email, retry_at));
        }
        let mut request = json!({
            "operation": operation,
            "email": email,
        });
        for source in [&credential, &fields] {
            let values = source.as_object().context("invalid mail request")?;
            for (key, value) in values {
                request[key] = value.clone();
            }
        }
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(HELPER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("Python 3 is required for Gmail IMAP accounts")?;
        let input = serde_json::to_vec(&request)?;
        child
            .stdin
            .take()
            .context("mail helper stdin unavailable")?
            .write_all(&input)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("mail helper stopped unexpectedly")
        }
        let response: HelperResponse = serde_json::from_slice(&output.stdout)
            .context("mail helper returned an invalid response")?;
        let mut response = response;
        let partial_quota = response
            .result
            .as_ref()
            .and_then(|result| result.get("error_kind"))
            .and_then(Value::as_str)
            == Some("rate_limited");
        if response.error_kind.as_deref() == Some("rate_limited") || partial_quota {
            let retry_at = chrono::Utc::now().timestamp() + 15 * 60;
            if let Some(cache) = &cache {
                cache.pause_imap(email, retry_at)?;
            }
            let message = imap_pause_message(email, retry_at);
            if partial_quota {
                response.result.as_mut().unwrap()["error"] = json!(message);
            } else {
                response.error = Some(message);
            }
        }
        if !response.ok {
            bail!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "Mail request failed".into())
            );
        }
        serde_json::from_value(response.result.unwrap_or(Value::Null))
            .context("mail helper returned unexpected data")
    }

    pub fn probe_credentials(email: &str, password: &str) -> Result<()> {
        Self::invoke(email, json!({"password": password}), "probe", json!({}))
    }

    pub fn goa_accounts() -> Result<Vec<GoaAccount>> {
        Self::invoke("", json!({}), "goa_accounts", json!({}))
    }

    pub fn probe_goa(email: &str, id: &str) -> Result<()> {
        Self::invoke(email, json!({"goa_id": id}), "probe", json!({}))
    }
}

impl ImapClient {
    pub fn for_account(store: AccountStore, email: &str) -> Result<Self> {
        let account = store.account(email)?;
        Ok(match account.kind {
            AccountKind::GmailAppPassword => Self::app_password(email.to_owned(), store),
            AccountKind::GnomeOnlineAccounts => Self::goa(
                email.to_owned(),
                account.goa_id.context("GOA account ID is missing")?,
                store,
            ),
        })
    }

    pub fn set_cancellation(&mut self, cancelled: Option<Arc<AtomicBool>>) {
        self.cancelled = cancelled;
    }

    pub fn labels(&mut self) -> Result<Vec<Label>> {
        self.call("labels", json!({}))
    }

    pub fn unread_counts(&mut self, labels: &[String]) -> Result<Vec<u32>> {
        self.call("unread_counts", json!({"labels": labels}))
    }

    pub fn poll_new_mail(&mut self, cursor: Option<&MailCursor>) -> Result<IncomingMailPoll> {
        self.call("poll_new_mail", json!({"cursor": cursor}))
    }

    pub fn list_threads(
        &mut self,
        label: Option<&str>,
        query: Option<&str>,
        _page_token: Option<&str>,
    ) -> Result<ThreadPage> {
        self.call(
            "list_threads",
            json!({"label": label, "query": query, "limit": 50}),
        )
    }

    pub fn list_threads_with_limit(
        &mut self,
        label: Option<&str>,
        query: Option<&str>,
        _page_token: Option<&str>,
        max_results: usize,
    ) -> Result<ThreadPage> {
        self.call(
            "list_threads",
            json!({"label": label, "query": query, "limit": max_results}),
        )
    }

    pub fn threads(&mut self, references: &[ThreadRef]) -> Result<Vec<Thread>> {
        self.call(
            "threads",
            json!({"ids": references.iter().map(|reference| &reference.id).collect::<Vec<_>>()}),
        )
    }

    pub fn attachment(&mut self, message_id: &str, body: &Body) -> Result<Vec<u8>> {
        let data = if let Some(data) = &body.data {
            data.clone()
        } else {
            self.call::<String>(
                "attachment",
                json!({
                    "id": message_id,
                    "attachment_id": body.attachment_id.as_deref().context("IMAP attachment path is unavailable")?,
                }),
            )?
        };
        Ok(crate::gmail::decode_attachment_data(&data)?)
    }

    pub fn inline_images(
        &mut self,
        message_id: &str,
        attachment_ids: &[String],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let encoded: HashMap<String, String> = self.call(
            "inline_images",
            json!({"id": message_id, "attachment_ids": attachment_ids}),
        )?;
        encoded
            .into_iter()
            .map(|(id, data)| Ok((id, crate::gmail::decode_attachment_data(&data)?)))
            .collect()
    }

    pub fn archive_thread(&mut self, id: &str) -> Result<()> {
        self.call("archive_thread", json!({"id": id}))
    }

    pub fn set_unread(&mut self, id: &str, value: bool) -> Result<()> {
        self.call("set_unread", json!({"id": id, "value": value}))
    }

    pub fn set_unread_many(&mut self, ids: &[String], value: bool) -> Result<BatchUpdateResult> {
        self.call("set_unread_many", json!({"ids": ids, "value": value}))
    }

    pub fn set_starred(&mut self, id: &str, value: bool) -> Result<()> {
        self.call("set_starred", json!({"id": id, "value": value}))
    }

    pub fn trash(&mut self, id: &str) -> Result<()> {
        self.call("trash", json!({"id": id}))
    }

    pub fn send(&mut self, message: &ComposeMessage) -> Result<()> {
        self.call(
            "send",
            json!({"raw": encode_message(&self.email, message)?}),
        )
    }

    pub fn create_draft(&mut self, message: &ComposeMessage) -> Result<()> {
        self.call(
            "create_draft",
            json!({"raw": encode_message(&self.email, message)?}),
        )
    }

    pub fn draft_for_message(&mut self, message_id: &str) -> Result<Draft> {
        self.call("draft_for_message", json!({"id": message_id}))
    }

    pub fn write_existing_draft(
        &mut self,
        id: &str,
        expected_message_id: &str,
        message: &ComposeMessage,
        send: bool,
    ) -> Result<()> {
        self.call(
            "write_existing_draft",
            json!({
                "id": id,
                "expected_message_id": expected_message_id,
                "raw": encode_message(&self.email, message)?,
                "send": send,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a mail-enabled Google account in GNOME Online Accounts and network access"]
    fn live_goa_read_only_round_trip() -> Result<()> {
        let email = std::env::var("POSTBIRD_GOA_TEST_EMAIL")
            .context("set POSTBIRD_GOA_TEST_EMAIL to a GOA Google Mail address")?;
        let account = ImapClient::goa_accounts()?
            .into_iter()
            .find(|account| account.email == email)
            .context("the requested Google Mail account is not available in GOA")?;
        ImapClient::probe_goa(&account.email, &account.id)?;
        let client = ImapClient::goa(account.email, account.id, AccountStore::open()?);
        let _: Vec<Label> = client.call("labels", json!({}))?;
        let page: ThreadPage =
            client.call("list_threads", json!({"label": "INBOX", "limit": 3}))?;
        if !page.threads.is_empty() {
            let ids = page
                .threads
                .iter()
                .map(|reference| &reference.id)
                .collect::<Vec<_>>();
            let threads: Vec<Thread> = client.call("threads", json!({"ids": ids}))?;
            assert_eq!(threads.len(), page.threads.len());
            assert!(threads.iter().all(|thread| !thread.messages.is_empty()));
        }
        Ok(())
    }
}
