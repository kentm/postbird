use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use lettre::message::{
    Attachment, Mailbox, Mailboxes, Message as MimeMessage, MultiPart, SinglePart,
    header::ContentType,
};
use reqwest::{
    Method,
    blocking::{Client, RequestBuilder, Response},
    header::CONTENT_LENGTH,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

use crate::{
    accounts::AccountStore,
    oauth::{OAuthCredentials, TokenSet},
};

const API_ROOT: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

pub struct GmailClient {
    http: Client,
    credentials: OAuthCredentials,
    token: TokenSet,
    email: String,
    store: AccountStore,
    cancelled: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Profile {
    #[serde(rename = "emailAddress")]
    pub email_address: String,
}

#[derive(Deserialize)]
struct GoogleErrorEnvelope {
    error: GoogleError,
}

#[derive(Deserialize)]
struct GoogleError {
    message: String,
    status: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ThreadPage {
    #[serde(default)]
    pub threads: Vec<ThreadRef>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ThreadRef {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Thread {
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LabelList {
    #[serde(default)]
    pub labels: Vec<Label>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Label {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "threadId", default)]
    pub thread_id: String,
    #[serde(rename = "labelIds", default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub payload: Payload,
    #[serde(rename = "internalDate", default)]
    pub internal_date: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Payload {
    #[serde(default)]
    pub filename: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub body: Body,
    #[serde(default)]
    pub parts: Vec<Payload>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Body {
    pub data: Option<String>,
    #[serde(rename = "attachmentId")]
    pub attachment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Draft {
    pub id: String,
    pub message: Message,
}

#[derive(Deserialize)]
struct DraftPage {
    #[serde(default)]
    drafts: Vec<DraftReference>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct DraftReference {
    id: String,
    message: MessageReference,
}

#[derive(Deserialize)]
struct MessageReference {
    id: String,
}

#[derive(Clone, Debug)]
pub struct ComposeMessage {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub html_body: Option<String>,
    pub in_reply_to: Option<String>,
    pub thread_id: Option<String>,
    pub attachments: Vec<PathBuf>,
    pub forwarded_attachments: Vec<ForwardedAttachment>,
}

#[derive(Clone, Debug)]
pub struct ForwardedAttachment {
    pub content_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub data: std::sync::Arc<[u8]>,
}

impl ForwardedAttachment {
    fn mime_part(&self) -> Result<SinglePart> {
        let content_type = ContentType::parse(&self.mime_type)
            .unwrap_or(ContentType::parse("application/octet-stream")?);
        let attachment = if let Some(id) = &self.content_id {
            Attachment::new_inline_with_name(id.clone(), self.filename.clone())
        } else {
            Attachment::new(self.filename.clone())
        };
        Ok(attachment.body(self.data.to_vec(), content_type))
    }
}

#[derive(Serialize)]
struct RawMessage<'a> {
    raw: &'a str,
    #[serde(rename = "threadId", skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
}

#[derive(Serialize)]
struct DraftMessage<'a> {
    message: RawMessage<'a>,
}

impl GmailClient {
    pub fn for_account(store: AccountStore, email: &str) -> Result<Self> {
        let credentials = store.credentials()?;
        let token = store.token(email)?;
        Self::new(credentials, token, email.to_owned(), store)
    }

    pub fn new(
        credentials: OAuthCredentials,
        token: TokenSet,
        email: String,
        store: AccountStore,
    ) -> Result<Self> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(30)).build()?,
            credentials,
            token,
            email,
            store,
            cancelled: None,
        })
    }

    pub fn set_cancellation(&mut self, cancelled: Option<Arc<AtomicBool>>) {
        self.cancelled = cancelled;
    }

    pub fn profile(&mut self) -> Result<Profile> {
        self.get("/profile", &[])
    }

    pub fn labels(&mut self) -> Result<Vec<Label>> {
        Ok(self.get::<LabelList>("/labels", &[])?.labels)
    }

    pub fn list_threads(
        &mut self,
        label: Option<&str>,
        query: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<ThreadPage> {
        self.list_threads_with_limit(label, query, page_token, 50)
    }

    pub fn list_threads_with_limit(
        &mut self,
        label: Option<&str>,
        query: Option<&str>,
        page_token: Option<&str>,
        max_results: usize,
    ) -> Result<ThreadPage> {
        let max_results = max_results.to_string();
        let mut parameters = vec![("maxResults", max_results.as_str())];
        if let Some(label) = label {
            parameters.push(("labelIds", label));
        }
        if let Some(query) = query {
            parameters.push(("q", query));
        }
        if let Some(token) = page_token {
            parameters.push(("pageToken", token));
        }
        self.get("/threads", &parameters)
    }

    pub fn thread(&mut self, id: &str) -> Result<Thread> {
        self.get(&format!("/threads/{id}"), &[("format", "full")])
    }

    pub fn attachment(&mut self, message_id: &str, body: &Body) -> Result<Vec<u8>> {
        let fetched;
        let data = if let Some(data) = &body.data {
            data
        } else if let Some(id) = &body.attachment_id {
            fetched = self.get::<Body>(&format!("/messages/{message_id}/attachments/{id}"), &[])?;
            fetched
                .data
                .as_ref()
                .context("attachment response has no data")?
        } else {
            anyhow::bail!("attachment has no data");
        };
        decode_attachment_data(data).context("attachment data is invalid")
    }

    pub fn archive_thread(&mut self, id: &str) -> Result<()> {
        let _: serde_json::Value = self.request(
            Method::POST,
            &format!("/threads/{id}/modify"),
            Some(&json!({"removeLabelIds": ["INBOX"]})),
        )?;
        Ok(())
    }

    pub fn set_unread(&mut self, id: &str, unread: bool) -> Result<()> {
        if unread {
            self.modify(id, &["UNREAD"], &[])
        } else {
            self.modify(id, &[], &["UNREAD"])
        }
    }

    pub fn set_starred(&mut self, id: &str, starred: bool) -> Result<()> {
        if starred {
            self.modify(id, &["STARRED"], &[])
        } else {
            self.modify(id, &[], &["STARRED"])
        }
    }

    pub fn trash(&mut self, id: &str) -> Result<()> {
        let _: serde_json::Value =
            self.request(Method::POST, &format!("/messages/{id}/trash"), None::<&()>)?;
        Ok(())
    }

    pub fn send(&mut self, message: &ComposeMessage) -> Result<()> {
        validate_send_recipients(message)?;
        let raw = Self::encode_message(&self.email, message)?;
        let _: serde_json::Value = self.request(
            Method::POST,
            "/messages/send",
            Some(&RawMessage {
                raw: &raw,
                thread_id: message.thread_id.as_deref(),
            }),
        )?;
        Ok(())
    }

    pub fn create_draft(&mut self, message: &ComposeMessage) -> Result<()> {
        let raw = Self::encode_message(&self.email, message)?;
        let _: serde_json::Value = self.request(
            Method::POST,
            "/drafts",
            Some(&DraftMessage {
                message: RawMessage {
                    raw: &raw,
                    thread_id: message.thread_id.as_deref(),
                },
            }),
        )?;
        Ok(())
    }

    pub fn draft_for_message(&mut self, message_id: &str) -> Result<Draft> {
        let mut token = None;
        loop {
            let mut query = vec![("maxResults", "500")];
            if let Some(token) = token.as_deref() {
                query.push(("pageToken", token));
            }
            let page: DraftPage = self.get("/drafts", &query)?;
            if let Some(draft) = page
                .drafts
                .iter()
                .find(|draft| draft.message.id == message_id)
            {
                let mut draft: Draft =
                    self.get(&format!("/drafts/{}", draft.id), &[("format", "full")])?;
                self.hydrate_draft_parts(&draft.message.id, &mut draft.message.payload)?;
                return Ok(draft);
            }
            token = page.next_page_token;
            if token.is_none() {
                bail!(
                    "This draft has changed or was deleted. Refresh the mailbox and open it again."
                );
            }
        }
    }

    fn hydrate_draft_parts(&mut self, id: &str, part: &mut Payload) -> Result<()> {
        if part.body.data.is_none() && part.body.attachment_id.is_some() {
            part.body.data = Some(URL_SAFE_NO_PAD.encode(self.attachment(id, &part.body)?));
        }
        for child in &mut part.parts {
            self.hydrate_draft_parts(id, child)?;
        }
        Ok(())
    }

    pub fn write_existing_draft(
        &mut self,
        id: &str,
        expected_message_id: &str,
        message: &ComposeMessage,
        send: bool,
    ) -> Result<()> {
        let current: Draft = self.get(&format!("/drafts/{id}"), &[("format", "minimal")])?;
        if current.message.id != expected_message_id {
            bail!(
                "This draft was edited elsewhere. Your edits are still here; reopen the latest draft before saving."
            );
        }
        if send {
            validate_send_recipients(message)?;
        }
        let raw = Self::encode_message(&self.email, message)?;
        let (method, path, body) =
            existing_draft_request(id, &raw, message.thread_id.as_deref(), send);
        let _: serde_json::Value = self.request(method, &path, Some(&body))?;
        Ok(())
    }

    fn encode_message(email: &str, message: &ComposeMessage) -> Result<String> {
        let mut builder = MimeMessage::builder()
            .keep_bcc()
            .from(email.parse::<Mailbox>().context("invalid sender address")?)
            .subject(&message.subject);
        if !message.to.trim().is_empty() {
            for recipient in message
                .to
                .parse::<Mailboxes>()
                .context("invalid recipient address")?
            {
                builder = builder.to(recipient);
            }
        }
        if message.to.trim().is_empty()
            && message.cc.trim().is_empty()
            && message.bcc.trim().is_empty()
        {
            // Gmail accepts recipientless drafts. Lettre's SMTP envelope is not
            // transmitted by this API; supply one without adding a To header.
            builder = builder.envelope(lettre::address::Envelope::new(None, vec![email.parse()?])?);
        }
        if !message.cc.trim().is_empty() {
            for recipient in message
                .cc
                .parse::<Mailboxes>()
                .context("invalid Cc address")?
            {
                builder = builder.cc(recipient);
            }
        }
        if !message.bcc.trim().is_empty() {
            for recipient in message
                .bcc
                .parse::<Mailboxes>()
                .context("invalid Bcc address")?
            {
                builder = builder.bcc(recipient);
            }
        }
        if let Some(reference) = &message.in_reply_to {
            builder = builder.in_reply_to(reference.clone());
        }
        let content = message
            .html_body
            .as_ref()
            .map(|html| MultiPart::alternative_plain_html(message.body.clone(), html.clone()));
        let mime = if message.attachments.is_empty() && message.forwarded_attachments.is_empty() {
            if let Some(content) = content {
                builder.multipart(content)?
            } else {
                builder.body(message.body.clone())?
            }
        } else {
            let mut multipart = if let Some(content) = content {
                MultiPart::mixed().multipart(content)
            } else {
                MultiPart::mixed().singlepart(SinglePart::plain(message.body.clone()))
            };
            for path in &message.attachments {
                let filename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("attachment");
                let content_type = mime_guess::from_path(path).first_or_octet_stream();
                multipart = multipart.singlepart(
                    Attachment::new(filename.to_owned()).body(
                        std::fs::read(path)
                            .with_context(|| format!("could not read {}", path.display()))?,
                        ContentType::parse(content_type.as_ref())?,
                    ),
                );
            }
            for attachment in &message.forwarded_attachments {
                multipart = multipart.singlepart(attachment.mime_part()?);
            }
            builder.multipart(multipart)?
        };
        Ok(URL_SAFE_NO_PAD.encode(mime.formatted()))
    }

    fn modify(&mut self, id: &str, add: &[&str], remove: &[&str]) -> Result<()> {
        let body = json!({ "addLabelIds": add, "removeLabelIds": remove });
        let _: serde_json::Value =
            self.request(Method::POST, &format!("/messages/{id}/modify"), Some(&body))?;
        Ok(())
    }

    fn get<T: DeserializeOwned>(&mut self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        retry_throttled(|| {
            crate::quota::acquire(
                &self.email,
                request_cost(&Method::GET, path),
                self.cancelled.as_deref(),
            )?;
            self.ensure_fresh_token()?;
            let response = self
                .http
                .get(format!("{API_ROOT}{path}"))
                .bearer_auth(&self.token.access_token)
                .query(query)
                .send()?;
            decode_response(response, &self.email)
        })
    }

    fn request<T: DeserializeOwned, B: Serialize + ?Sized>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        // Retry only explicit Gmail quota rejections. Transport errors and other
        // ambiguous outcomes are never replayed, especially for sends.
        retry_throttled(|| {
            crate::quota::acquire(
                &self.email,
                request_cost(&method, path),
                self.cancelled.as_deref(),
            )?;
            self.ensure_fresh_token()?;
            let request = self
                .http
                .request(method.clone(), format!("{API_ROOT}{path}"))
                .bearer_auth(&self.token.access_token);
            decode_response(with_optional_json_body(request, body).send()?, &self.email)
        })
    }

    fn ensure_fresh_token(&mut self) -> Result<()> {
        if !self.token.expires_soon() {
            return Ok(());
        }
        let refresh_token = self
            .token
            .refresh_token
            .as_deref()
            .context("this account has no refresh token; connect it again")?;
        self.token = self.credentials.refresh(refresh_token)?;
        self.store.save_token(&self.email, &self.token)
    }
}

fn validate_send_recipients(message: &ComposeMessage) -> Result<()> {
    if message.to.trim().is_empty() && message.cc.trim().is_empty() && message.bcc.trim().is_empty()
    {
        bail!("Add a recipient before sending.");
    }
    Ok(())
}

fn existing_draft_request(
    id: &str,
    raw: &str,
    thread_id: Option<&str>,
    send: bool,
) -> (Method, String, serde_json::Value) {
    let body = json!({"id": id, "message": {"raw": raw, "threadId": thread_id}});
    if send {
        (Method::POST, "/drafts/send".to_owned(), body)
    } else {
        (Method::PUT, format!("/drafts/{id}"), body)
    }
}

fn with_optional_json_body<B: Serialize + ?Sized>(
    request: RequestBuilder,
    body: Option<&B>,
) -> RequestBuilder {
    match body {
        Some(body) => request.json(body),
        None => request.header(CONTENT_LENGTH, 0),
    }
}

// Costs from Google's Gmail API quota table (September 2026).
fn request_cost(method: &Method, path: &str) -> u32 {
    if method == Method::GET {
        if path == "/profile" || path == "/labels" {
            1
        } else if path == "/threads" {
            10
        } else if path.starts_with("/threads/") {
            40
        } else if path == "/drafts" {
            5
        } else {
            20
        }
    } else if path.ends_with("/send") {
        100
    } else if path == "/drafts" {
        10
    } else if path.starts_with("/drafts/") && method == Method::PUT {
        15
    } else if path.ends_with("/modify") {
        if path.starts_with("/threads/") { 10 } else { 5 }
    } else if path.ends_with("/trash") {
        20
    } else {
        100
    }
}

fn retry_after_delay(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            chrono::DateTime::parse_from_rfc2822(value)
                .ok()
                .map(|date| {
                    (date.with_timezone(&chrono::Utc) - chrono::Utc::now())
                        .to_std()
                        .unwrap_or_default()
                })
        })
}

fn is_quota_error(status: u16, body: &str) -> bool {
    if status == 429 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("ratelimitexceeded")
        || lower.contains("quota exceeded")
        || lower.contains("quotaexceeded")
        || lower.contains("resource_exhausted")
}

pub const QUOTA_PENDING: &str =
    "Gmail is temporarily busy. Please try again later; unsaved edits remain open.";

#[derive(Debug)]
struct QuotaRejected;
impl std::fmt::Display for QuotaRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(QUOTA_PENDING)
    }
}
impl std::error::Error for QuotaRejected {}

fn retry_throttled<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    for attempt in 0..4 {
        match operation() {
            Err(error) if error.is::<QuotaRejected>() && attempt < 3 => continue,
            result => return result,
        }
    }
    unreachable!()
}

fn decode_response<T: DeserializeOwned>(response: Response, account: &str) -> Result<T> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(retry_after_delay)
        .unwrap_or_default();
    let body = response.text()?;
    if is_quota_error(status.as_u16(), &body) {
        crate::quota::reject(
            account,
            retry_after + Duration::from_millis(rand::random::<u16>() as u64 % 1000),
        )?;
        return Err(QuotaRejected.into());
    }
    if !status.is_success() {
        if let Ok(envelope) = serde_json::from_str::<GoogleErrorEnvelope>(&body) {
            let kind = envelope.error.status.unwrap_or_else(|| status.to_string());
            bail!("Google API {kind}: {}", envelope.error.message);
        }
        bail!("Google API returned {status}: {body}");
    }
    serde_json::from_str(&body).context("Google returned an invalid response")
}

impl Message {
    pub fn attachments(&self) -> Vec<&Payload> {
        fn collect<'a>(part: &'a Payload, found: &mut Vec<&'a Payload>) {
            if is_attachment(part) {
                found.push(part);
            } else {
                for child in &part.parts {
                    collect(child, found);
                }
            }
        }
        let mut found = Vec::new();
        collect(&self.payload, &mut found);
        found
    }

    pub fn header(&self, name: &str) -> &str {
        self.payload
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
            .unwrap_or_default()
    }

    pub fn body_text(&self) -> String {
        if let Some(plain) = find_body(&self.payload, "text/plain") {
            return plain;
        }
        find_body(&self.payload, "text/html")
            .and_then(|html| html2text::from_read(html.as_bytes(), 100).ok())
            .unwrap_or_default()
    }

    pub fn body_html(&self) -> Option<String> {
        find_body(&self.payload, "text/html")
    }

    pub fn rendered_body(&self) -> String {
        if let Some(html) = self.body_html() {
            return render_html_document(html);
        }
        format!(
            r#"<!doctype html><html><head><meta charset="utf-8">{RENDERER_STYLE}</head><body><pre class="plain">{}</pre></body></html>"#,
            escape_html(&self.body_text())
        )
    }
}

const RENDERER_STYLE: &str = r#"<style id="postbird-renderer">
html, body { overflow-wrap: anywhere; }
img { max-width: 100%; height: auto; }
pre.plain { white-space: pre-wrap; font: 15px system-ui, sans-serif; margin: 0; padding: 1rem; }
blockquote { margin-inline: .5rem 0; padding-inline-start: 1rem; border-inline-start: 3px solid #8888; }
</style>"#;

fn render_html_document(mut html: String) -> String {
    let lower = html.to_ascii_lowercase();
    let is_document = lower.contains("<!doctype") || lower.contains("<html");
    if !is_document {
        return format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">{RENDERER_STYLE}</head><body>{html}</body></html>"
        );
    }
    if let Some(position) = lower.find("</head>") {
        html.insert_str(position, RENDERER_STYLE);
    } else if let Some(position) = lower.find("<body") {
        html.insert_str(position, RENDERER_STYLE);
    } else {
        html.insert_str(0, RENDERER_STYLE);
    }
    html
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn is_attachment(payload: &Payload) -> bool {
    !payload.filename.is_empty()
        || payload
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("Content-ID"))
        || payload.headers.iter().any(|header| {
            header.name.eq_ignore_ascii_case("Content-Disposition")
                && header
                    .value
                    .split(';')
                    .next()
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("attachment"))
        })
}

fn find_body(payload: &Payload, wanted_type: &str) -> Option<String> {
    if is_attachment(payload) {
        return None;
    }
    if payload.mime_type == wanted_type
        && let Some(data) = &payload.body.data
        && let Ok(bytes) = decode_attachment_data(data)
    {
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    payload
        .parts
        .iter()
        .find_map(|part| find_body(part, wanted_type))
}

pub fn decode_attachment_data(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    URL_SAFE
        .decode(data)
        .or_else(|_| URL_SAFE_NO_PAD.decode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_quota_rejections_are_retried() {
        let mut calls = 0;
        let result = retry_throttled(|| {
            calls += 1;
            if calls < 3 {
                Err(QuotaRejected.into())
            } else {
                Ok("loaded")
            }
        })
        .unwrap();
        assert_eq!(result, "loaded");
        assert_eq!(calls, 3);
        let mut sends = 0;
        let failed: Result<()> = retry_throttled(|| {
            sends += 1;
            bail!("connection lost after sending");
        });
        assert!(failed.is_err());
        assert_eq!(sends, 1);
        let mut rejections = 0;
        let failed: Result<()> = retry_throttled(|| {
            rejections += 1;
            Err(QuotaRejected.into())
        });
        assert!(failed.unwrap_err().is::<QuotaRejected>());
        assert_eq!(rejections, 4);
    }

    #[test]
    fn quota_errors_are_distinct_from_missing_permissions() {
        assert!(is_quota_error(
            403,
            r#"{"error":{"status":"PERMISSION_DENIED","message":"Quota exceeded for quota metric 'Total Query Cost'"}}"#
        ));
        assert!(is_quota_error(
            403,
            r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#
        ));
        assert!(is_quota_error(429, "Too many requests"));
        assert!(!is_quota_error(
            403,
            r#"{"error":{"message":"Insufficient Permission"}}"#
        ));
        assert!(!is_quota_error(500, "server error"));
        assert_eq!(retry_after_delay("120"), Some(Duration::from_secs(120)));
        assert!(retry_after_delay("not a date").is_none());
        assert_eq!(
            retry_after_delay("Wed, 01 Jan 2020 00:00:00 GMT"),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn mailbox_and_draft_operations_charge_documented_units() {
        assert_eq!(
            request_cost(&Method::GET, "/threads") + 50 * request_cost(&Method::GET, "/threads/id"),
            2010
        );
        assert_eq!(request_cost(&Method::GET, "/drafts/id"), 20);
        assert_eq!(
            request_cost(&Method::GET, "/messages/id/attachments/attachment"),
            20
        );
        assert_eq!(request_cost(&Method::PUT, "/drafts/id"), 15);
        assert_eq!(request_cost(&Method::POST, "/drafts/send"), 100);
        assert_eq!(request_cost(&Method::POST, "/messages/send"), 100);
    }

    #[test]
    fn draft_update_and_send_use_the_stable_draft_id() {
        let (method, path, body) =
            existing_draft_request("draft-123", "edited-mime", Some("thread-1"), false);
        assert_eq!(method, Method::PUT);
        assert_eq!(path, "/drafts/draft-123");
        assert_eq!(
            body,
            json!({"id": "draft-123", "message": {"raw": "edited-mime", "threadId": "thread-1"}})
        );
        let (method, path, sent) =
            existing_draft_request("draft-123", "edited-mime", Some("thread-1"), true);
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/drafts/send");
        assert_eq!(sent, body);
        let minimal: Draft = serde_json::from_value(
            json!({"id": "draft-123", "message": {"id": "replacement-message"}}),
        )
        .unwrap();
        assert_eq!(minimal.message.id, "replacement-message");
        let empty_page: DraftPage = serde_json::from_value(json!({})).unwrap();
        assert!(empty_page.drafts.is_empty());
    }

    #[test]
    fn drafts_allow_no_recipients_and_preserve_bcc_and_inline_images() {
        let mut message = ComposeMessage {
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Draft".to_owned(),
            body: "Hello".to_owned(),
            html_body: Some("<p>Hello<img src=\"cid:image-1\"></p>".to_owned()),
            in_reply_to: None,
            thread_id: None,
            attachments: Vec::new(),
            forwarded_attachments: vec![ForwardedAttachment {
                content_id: Some("image-1".to_owned()),
                filename: "image.png".to_owned(),
                mime_type: "image/png".to_owned(),
                data: std::sync::Arc::from(&b"image-bytes"[..]),
            }],
        };
        let mime = |message: &ComposeMessage| {
            String::from_utf8(
                decode_attachment_data(
                    &GmailClient::encode_message("sender@example.com", message).unwrap(),
                )
                .unwrap(),
            )
            .unwrap()
        };
        let raw = mime(&message);
        assert!(!raw.contains("To:"));
        assert!(validate_send_recipients(&message).is_err());
        message.bcc = "hidden@example.com".to_owned();
        let raw = mime(&message);
        assert!(raw.contains("Bcc: hidden@example.com"));
        assert!(raw.contains("Content-ID: <image-1>"));
        assert!(raw.contains("inline"));
        assert!(validate_send_recipients(&message).is_ok());
    }

    #[test]
    fn forwards_multiple_attachments_in_the_encoded_message() {
        let message = ComposeMessage {
            to: "recipient@example.com".to_owned(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Fwd: Report".to_owned(),
            body: "Original message".to_owned(),
            html_body: Some("<p>Original message</p>".to_owned()),
            in_reply_to: None,
            thread_id: None,
            attachments: Vec::new(),
            forwarded_attachments: vec![
                ForwardedAttachment {
                    content_id: None,
                    filename: "report.pdf".to_owned(),
                    mime_type: "application/pdf".to_owned(),
                    data: std::sync::Arc::from(&b"%PDF-test"[..]),
                },
                ForwardedAttachment {
                    content_id: None,
                    filename: "report.pdf".to_owned(),
                    mime_type: "application/pdf".to_owned(),
                    data: std::sync::Arc::from(&b"%PDF-other"[..]),
                },
            ],
        };
        let raw = GmailClient::encode_message("sender@example.com", &message).unwrap();
        let mime = String::from_utf8(decode_attachment_data(&raw).unwrap()).unwrap();
        assert!(mime.contains("multipart/mixed"));
        assert!(mime.contains("multipart/alternative"));
        assert_eq!(mime.matches("filename=\"report.pdf\"").count(), 2);
        assert!(mime.contains("%PDF-test"));
        assert!(mime.contains("%PDF-other"));
        assert!(!mime.contains("In-Reply-To:"));
    }

    #[test]
    fn finds_nested_attachments_without_using_them_as_the_message_body() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "1", "threadId": "t", "payload": {
                "mimeType": "multipart/mixed", "parts": [
                    {"mimeType": "text/plain", "filename": "notes.txt", "body": {"data": "ZmlsZQ=="}},
                    {"mimeType": "multipart/alternative", "parts": [
                        {"mimeType": "text/plain", "body": {"data": "SGVsbG8"}},
                        {"mimeType": "application/pdf", "filename": "report.pdf", "body": {"attachmentId": "external"}},
                        {"mimeType": "application/octet-stream", "headers": [{"name": "Content-Disposition", "value": "Attachment; filename=missing"}], "body": {"data": ""}}
                    ]}
                ]
            }
        })).unwrap();
        assert_eq!(message.body_text(), "Hello");
        let attachments = message.attachments();
        assert_eq!(attachments.len(), 3);
        assert_eq!(attachments[0].filename, "notes.txt");
        assert_eq!(
            attachments[1].body.attachment_id.as_deref(),
            Some("external")
        );
        assert_eq!(
            decode_attachment_data(attachments[0].body.data.as_ref().unwrap()).unwrap(),
            b"file"
        );
        assert_eq!(decode_attachment_data("").unwrap(), b"");
        assert!(decode_attachment_data("!").is_err());
        let cached: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(cached.attachments()[1].filename, "report.pdf");
    }

    #[test]
    fn bodyless_posts_send_an_explicit_zero_content_length() {
        let request = with_optional_json_body(
            Client::new().post("https://example.com/messages/id/trash"),
            None::<&()>,
        )
        .build()
        .unwrap();

        assert_eq!(request.headers()[CONTENT_LENGTH], "0");
    }

    #[test]
    fn extracts_nested_plain_body() {
        let message: Message = serde_json::from_value(json!({
            "id": "1", "threadId": "t", "payload": {
                "mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/plain", "body": {"data": "SGVsbG8"}}
                ]
            }
        }))
        .unwrap();
        assert_eq!(message.body_text(), "Hello");
    }

    #[test]
    fn renders_html_and_escapes_plain_text() {
        let html: Message = serde_json::from_value(json!({
            "id": "1", "threadId": "t", "payload": {
                "mimeType": "text/html", "body": {"data": "PGI-SGk8L2I-"}
            }
        }))
        .unwrap();
        assert!(html.rendered_body().contains("<b>Hi</b>"));

        let plain: Message = serde_json::from_value(json!({
            "id": "2", "threadId": "t", "payload": {
                "mimeType": "text/plain", "body": {"data": "PHNjcmlwdD4"}
            }
        }))
        .unwrap();
        assert!(plain.rendered_body().contains("&lt;script&gt;"));
    }

    #[test]
    fn preserves_complete_html_documents() {
        let rendered = render_html_document(
            "<!DOCTYPE html><html><head><style>.email{color:red}</style></head><body class=email>Hi</body></html>".to_owned(),
        );
        assert_eq!(rendered.matches("<!DOCTYPE").count(), 1);
        assert_eq!(rendered.matches("<html").count(), 1);
        assert!(rendered.contains("postbird-renderer"));
        assert!(rendered.contains(".email{color:red}"));
    }

    #[test]
    fn decodes_padded_and_unpadded_gmail_bodies() {
        assert_eq!(
            decode_attachment_data("PGI-SGk8L2I-").unwrap(),
            b"<b>Hi</b>"
        );
        assert_eq!(
            decode_attachment_data("PGI-SGk8L2I-PC9wPg==").unwrap(),
            b"<b>Hi</b></p>"
        );
    }
}
