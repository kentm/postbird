use std::{path::PathBuf, time::Duration};

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
    blocking::{Client, Response},
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
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "labelIds", default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    pub payload: Payload,
    #[serde(rename = "internalDate", default)]
    pub internal_date: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Payload {
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
        })
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

    pub fn archive(&mut self, id: &str) -> Result<()> {
        self.modify(id, &[], &["INBOX"])
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
        let raw = self.encode_message(message)?;
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
        let raw = self.encode_message(message)?;
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

    fn encode_message(&self, message: &ComposeMessage) -> Result<String> {
        let mut builder = MimeMessage::builder()
            .from(
                self.email
                    .parse::<Mailbox>()
                    .context("invalid sender address")?,
            )
            .subject(&message.subject);
        for recipient in message
            .to
            .parse::<Mailboxes>()
            .context("invalid recipient address")?
        {
            builder = builder.to(recipient);
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
        let mime = if message.attachments.is_empty() {
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
        self.ensure_fresh_token()?;
        let response = self
            .http
            .get(format!("{API_ROOT}{path}"))
            .bearer_auth(&self.token.access_token)
            .query(query)
            .send()?;
        decode_response(response)
    }

    fn request<T: DeserializeOwned, B: Serialize + ?Sized>(
        &mut self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        self.ensure_fresh_token()?;
        let mut request = self
            .http
            .request(method, format!("{API_ROOT}{path}"))
            .bearer_auth(&self.token.access_token);
        if let Some(body) = body {
            request = request.json(body);
        }
        decode_response(request.send()?)
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

fn decode_response<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let body = response.text()?;
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

fn find_body(payload: &Payload, wanted_type: &str) -> Option<String> {
    if payload.mime_type == wanted_type
        && let Some(data) = &payload.body.data
        && let Ok(bytes) = decode_gmail_body(data)
    {
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    payload
        .parts
        .iter()
        .find_map(|part| find_body(part, wanted_type))
}

fn decode_gmail_body(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    URL_SAFE
        .decode(data)
        .or_else(|_| URL_SAFE_NO_PAD.decode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(decode_gmail_body("PGI-SGk8L2I-").unwrap(), b"<b>Hi</b>");
        assert_eq!(
            decode_gmail_body("PGI-SGk8L2I-PC9wPg==").unwrap(),
            b"<b>Hi</b></p>"
        );
    }
}
