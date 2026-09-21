use std::path::PathBuf;

use anyhow::{Context, Result};
use base64::{
    Engine as _,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use lettre::message::{
    Attachment, Mailbox, Mailboxes, Message as MimeMessage, MultiPart, SinglePart,
    header::ContentType,
};
use serde::{Deserialize, Serialize};

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

pub(crate) fn encode_message(email: &str, message: &ComposeMessage) -> Result<String> {
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
    if message.to.trim().is_empty() && message.cc.trim().is_empty() && message.bcc.trim().is_empty()
    {
        // Lettre needs an envelope for recipientless drafts, even though
        // only the formatted MIME message is passed to the IMAP helper.
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

    pub fn attachment_groups(&self) -> (Vec<&Payload>, Vec<&Payload>) {
        let mut files = Vec::new();
        let mut inline_images = Vec::new();
        for part in self.attachments() {
            if is_inline_image(part) {
                inline_images.push(part);
            } else {
                files.push(part);
            }
        }
        (files, inline_images)
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
        || is_inline_image(payload)
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

fn is_inline_image(payload: &Payload) -> bool {
    if !payload
        .mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .starts_with("image/")
    {
        return false;
    }
    let disposition = payload
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("Content-Disposition"))
        .and_then(|header| header.value.split(';').next())
        .map(str::trim);
    if disposition.is_some_and(|value| value.eq_ignore_ascii_case("attachment")) {
        return false;
    }
    disposition.is_some_and(|value| value.eq_ignore_ascii_case("inline"))
        || payload
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("Content-ID"))
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
    use serde_json::json;

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
                decode_attachment_data(&encode_message("sender@example.com", message).unwrap())
                    .unwrap(),
            )
            .unwrap()
        };
        let raw = mime(&message);
        assert!(!raw.contains("To:"));
        message.bcc = "hidden@example.com".to_owned();
        let raw = mime(&message);
        assert!(raw.contains("Bcc: hidden@example.com"));
        assert!(raw.contains("Content-ID: <image-1>"));
        assert!(raw.contains("inline"));
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
        let raw = encode_message("sender@example.com", &message).unwrap();
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
    fn groups_inline_images_without_hiding_explicit_attachments() {
        let message: Message = serde_json::from_value(json!({
            "id": "1", "threadId": "t", "payload": {
                "mimeType": "multipart/mixed", "parts": [
                    {"mimeType": "application/pdf", "filename": "report.pdf"},
                    {"mimeType": "image/png", "filename": "footer.png", "headers": [
                        {"name": "Content-Disposition", "value": "inline; filename=footer.png"}
                    ]},
                    {"mimeType": "image/png", "filename": "logo.png", "headers": [
                        {"name": "Content-ID", "value": "<logo@example.com>"}
                    ]},
                    {"mimeType": "image/png", "filename": "photo.png", "headers": [
                        {"name": "Content-ID", "value": "<photo@example.com>"},
                        {"name": "Content-Disposition", "value": "attachment; filename=photo.png"}
                    ]},
                    {"mimeType": "image/jpeg", "filename": "unlabelled.jpg"},
                    {"mimeType": "image/gif", "headers": [
                        {"name": "Content-Disposition", "value": "inline"}
                    ]}
                ]
            }
        }))
        .unwrap();

        let (files, inline_images) = message.attachment_groups();
        assert_eq!(
            files
                .iter()
                .map(|part| part.filename.as_str())
                .collect::<Vec<_>>(),
            ["report.pdf", "photo.png", "unlabelled.jpg"]
        );
        assert_eq!(
            inline_images
                .iter()
                .map(|part| part.filename.as_str())
                .collect::<Vec<_>>(),
            ["footer.png", "logo.png", ""]
        );
        assert_eq!(message.attachments().len(), 6);
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
