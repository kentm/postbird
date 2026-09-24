use std::{collections::HashMap, path::PathBuf};

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
    pub from: Option<String>,
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
        .from(
            message
                .from
                .as_deref()
                .unwrap_or(email)
                .parse::<Mailbox>()
                .context("invalid sender address")?,
        )
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
    let content = if let Some(html) = &message.html_body {
        let alternative =
            MultiPart::alternative().singlepart(SinglePart::plain(message.body.clone()));
        let images = message
            .forwarded_attachments
            .iter()
            .filter(|file| file.content_id.is_some())
            .collect::<Vec<_>>();
        Some(if images.is_empty() {
            alternative.singlepart(SinglePart::html(html.clone()))
        } else {
            let mut related = MultiPart::related().singlepart(SinglePart::html(html.clone()));
            for image in images {
                related = related.singlepart(image.mime_part()?);
            }
            alternative.multipart(related)
        })
    } else {
        None
    };
    let files = message
        .forwarded_attachments
        .iter()
        .filter(|file| file.content_id.is_none() || message.html_body.is_none())
        .collect::<Vec<_>>();
    let mime = if message.attachments.is_empty() && files.is_empty() {
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
        for attachment in files {
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

    pub fn inline_image_parts(&self) -> Vec<(&Payload, &str)> {
        self.attachment_groups()
            .1
            .into_iter()
            .filter_map(|part| {
                let id = part
                    .headers
                    .iter()
                    .find(|header| header.name.eq_ignore_ascii_case("Content-ID"))?
                    .value
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .trim();
                (!id.is_empty()).then_some((part, id))
            })
            .collect()
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

    pub fn preview(&self) -> String {
        let snippet = preview_text(&self.snippet);
        // IMAP snippets may be the first 160 characters of raw HTML, cut off
        // inside a tag or stylesheet. Use the complete cached body in that case.
        let contains_markup = self.snippet.split('<').skip(1).any(|tail| {
            tail.starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '!' | '/' | '?'))
        });
        if readable_preview(&snippet) && !contains_markup {
            return snippet;
        }
        if let Some(html) = self.body_html()
            && let Some(preview) = html_body_preview(&html)
        {
            return preview;
        }
        find_body(&self.payload, "text/plain")
            .map(|plain| normalize_preview(&plain))
            .filter(|plain| readable_preview(plain))
            .unwrap_or_default()
    }

    pub fn needs_body_refresh(&self) -> bool {
        fn deferred_body(part: &Payload) -> bool {
            if is_attachment(part) {
                return false;
            }
            // Older caches misclassified body parts with Content-ID headers
            // as downloadable attachments, leaving their content out entirely.
            (is_body_part(part) && part.body.data.is_none() && part.body.attachment_id.is_some())
                || part.parts.iter().any(deferred_body)
        }
        deferred_body(&self.payload)
    }

    pub fn rendered_body_with_images(
        &self,
        inline_images: &HashMap<String, String>,
        load_remote_images: bool,
    ) -> String {
        if let Some(html) = self.body_html() {
            return render_html_document(
                replace_cid_urls(&html, inline_images),
                load_remote_images,
            );
        }
        format!(
            r#"<!doctype html><html><head><meta charset="utf-8">{RENDERER_STYLE}</head><body><pre class="plain">{}</pre></body></html>"#,
            escape_html(&self.body_text())
        )
    }
}

fn normalize_preview(text: &str) -> String {
    text.replace(['\u{200b}', '\u{feff}', '\u{ad}'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}

fn readable_preview(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

fn preview_text(html: &str) -> String {
    normalize_preview(
        &html2text::config::with_decorator(html2text::render::TrivialDecorator::new())
            .string_from_read(html.as_bytes(), html.len().max(200))
            .unwrap_or_else(|_| html.to_owned()),
    )
}

fn html_body_preview(html: &str) -> Option<String> {
    let config = html2text::config::with_decorator(html2text::render::TrivialDecorator::new());
    let dom = config.parse_html(html.as_bytes()).ok()?;
    let mut pending = vec![dom.document.clone()];
    let mut blocks = Vec::new();
    while let Some(node) = pending.pop() {
        if let html2text::Element { name, attrs, .. } = &node.data {
            if matches!(
                name.local.as_ref(),
                "head" | "script" | "style" | "template"
            ) {
                node.children.borrow_mut().clear();
                continue;
            }
            let hidden = attrs.borrow().iter().any(|attr| {
                let value = attr.value.to_ascii_lowercase();
                attr.name.local.as_ref() == "hidden"
                    || (attr.name.local.as_ref() == "aria-hidden" && value == "true")
                    || (attr.name.local.as_ref() == "style"
                        && value.split(';').any(|declaration| {
                            let Some((property, value)) = declaration.split_once(':') else {
                                return false;
                            };
                            matches!(
                                (
                                    property.trim(),
                                    value.trim().trim_end_matches("!important").trim()
                                ),
                                ("display", "none") | ("visibility", "hidden")
                            )
                        }))
            });
            if hidden {
                // Also remove hidden preheaders from the whole-body fallback.
                node.children.borrow_mut().clear();
                continue;
            }
            if matches!(
                name.local.as_ref(),
                "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            ) {
                blocks.push(node.clone());
            }
        }
        pending.extend(node.children.borrow().iter().rev().cloned());
    }
    for node in blocks {
        let block = html2text::RcDom {
            document: node,
            ..Default::default()
        };
        let text = config
            .dom_to_render_tree(&block)
            .ok()
            .and_then(|tree| config.render_to_string(tree, html.len().max(200)).ok())
            .map(|text| normalize_preview(&text));
        if let Some(text) = text.filter(|text| readable_preview(text)) {
            return Some(text);
        }
    }
    config
        .dom_to_render_tree(&dom)
        .ok()
        .and_then(|tree| config.render_to_string(tree, html.len().max(200)).ok())
        .map(|text| normalize_preview(&text))
        .filter(|text| readable_preview(text))
}

const RENDERER_STYLE: &str = r#"<style id="postbird-renderer">
html, body { overflow-wrap: anywhere; }
/* Mail often fixes both dimensions; override the height when narrowing an image. */
img { max-width: min(100%, 100vw) !important; min-width: 0 !important; height: auto !important; }
pre.plain { white-space: pre-wrap; font: 15px system-ui, sans-serif; margin: 0; padding: 1rem; }
blockquote { margin-inline: .5rem 0; padding-inline-start: 1rem; border-inline-start: 3px solid #8888; }
</style>"#;

fn render_html_document(mut html: String, load_remote_images: bool) -> String {
    let image_sources = if load_remote_images {
        "data: http: https:"
    } else {
        "data:"
    };
    // Local CID images become data URLs. Keep all other network resources blocked
    // unless the user explicitly enables remote images.
    let head = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src {image_sources}; style-src 'unsafe-inline'; font-src data:; base-uri 'none'; form-action 'none'\">{RENDERER_STYLE}"
    );
    let lower = html.to_ascii_lowercase();
    let is_document = lower.contains("<!doctype") || lower.contains("<html");
    if !is_document {
        return format!(
            "<!doctype html><html><head><meta charset=\"utf-8\">{head}</head><body>{html}</body></html>"
        );
    }
    if let Some(position) = lower
        .find("<head")
        .and_then(|start| lower[start..].find('>').map(|end| start + end + 1))
    {
        html.insert_str(position, &head);
    } else if let Some(position) = lower
        .find("<html")
        .and_then(|start| lower[start..].find('>').map(|end| start + end + 1))
    {
        html.insert_str(position, &format!("<head>{head}</head>"));
    } else if let Some(position) = lower.find("<body") {
        html.insert_str(position, &format!("<head>{head}</head>"));
    } else {
        html.insert_str(0, &format!("<head>{head}</head>"));
    }
    html
}

fn replace_cid_urls(html: &str, images: &HashMap<String, String>) -> String {
    if images.is_empty() {
        return html.to_owned();
    }
    let lower = html.to_ascii_lowercase();
    let mut rendered = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("cid:") {
        let start = cursor + offset;
        rendered.push_str(&html[cursor..start]);
        let id_start = start + 4;
        let id_end = html[id_start..]
            .find(|character: char| {
                matches!(
                    character,
                    '"' | '\'' | '<' | '>' | ')' | ' ' | '\n' | '\r' | '\t'
                )
            })
            .map_or(html.len(), |end| id_start + end);
        let original = &html[start..id_end];
        let id = percent_decode(&html[id_start..id_end])
            .unwrap_or_else(|| html[id_start..id_end].to_owned())
            .to_ascii_lowercase();
        rendered.push_str(images.get(&id).map_or(original, String::as_str));
        cursor = id_end;
    }
    rendered.push_str(&html[cursor..]);
    rendered
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut remaining = value.as_bytes().iter().copied();
    while let Some(byte) = remaining.next() {
        if byte == b'%' {
            let high = (remaining.next()? as char).to_digit(16)?;
            let low = (remaining.next()? as char).to_digit(16)?;
            bytes.push(((high << 4) | low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn is_body_part(payload: &Payload) -> bool {
    let mime_type = payload
        .mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(mime_type.as_str(), "text/plain" | "text/html") || mime_type.starts_with("multipart/")
}

fn is_attachment(payload: &Payload) -> bool {
    !payload.filename.is_empty()
        || is_inline_image(payload)
        || (!is_body_part(payload)
            && payload
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("Content-ID")))
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
    fn previews_recover_from_truncated_html_using_readable_blocks() {
        for (snippet, html, expected) in [
            (
                "<",
                "<p>Hello <b>world</b> &amp; friends</p>",
                "Hello world & friends",
            ),
            (
                "<!doctype html><html><head><style>body {",
                "<head><title>Ignore me</title></head><h2>September round-up</h2><p>More news</p>",
                "September round-up",
            ),
            (
                "",
                "<p>&nbsp;&#8203;</p><p>&lt;</p><h6>First heading</h6>",
                "First heading",
            ),
            (
                "<",
                "<p>First paragraph</p><h1>Later heading</h1>",
                "First paragraph",
            ),
            (
                "<",
                "<div hidden><h1>Hidden</h1></div><p style='display: none !important'>Hidden too</p><p><span style='visibility:hidden'>Secret</span>Visible</p>",
                "Visible",
            ),
            (
                "<",
                "<style>p {color:red}</style><script>ignored()</script><div>Text without a paragraph</div>",
                "Text without a paragraph",
            ),
            (
                "<",
                "<p>Read <a href='https://example.com'>the news</a></p>",
                "Read the news",
            ),
            (
                "Hello &amp; welcome\nagain",
                "<h1>Other text</h1>",
                "Hello & welcome again",
            ),
            ("<", "<p>你好世界</p>", "你好世界"),
            ("<", "<img src='banner.png'>", ""),
        ] {
            let message: Message = serde_json::from_value(json!({
                "id": "preview",
                "snippet": snippet,
                "payload": {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode(html)}}
            }))
            .unwrap();
            assert_eq!(message.preview(), expected, "HTML: {html}");
        }
    }

    #[test]
    fn previews_fall_back_to_plain_text_and_bound_unicode_length() {
        let message: Message = serde_json::from_value(json!({
            "id": "plain",
            "snippet": "",
            "payload": {"mimeType": "text/plain", "body": {"data": URL_SAFE_NO_PAD.encode("A < B & C\nNext line")}}
        })).unwrap();
        assert_eq!(message.preview(), "A < B & C Next line");
        let html = format!("<p>{}</p>", "界".repeat(200));
        assert_eq!(html_body_preview(&html).unwrap(), "界".repeat(160));
    }

    #[test]
    fn drafts_allow_no_recipients_and_preserve_bcc_and_inline_images() {
        let mut message = ComposeMessage {
            from: None,
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
        assert!(raw.contains("From: sender@example.com"));
        message.from = Some("alias@example.com".into());
        message.bcc = "hidden@example.com".to_owned();
        let raw = mime(&message);
        assert!(raw.contains("From: alias@example.com"));
        assert!(!raw.contains("From: sender@example.com"));
        assert!(raw.contains("Bcc: hidden@example.com"));
        assert!(raw.contains("Content-ID: <image-1>"));
        assert!(raw.contains("inline"));
        assert!(raw.contains("multipart/related"));
    }

    #[test]
    fn inline_images_and_file_attachments_have_distinct_mime_parts() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let path =
            std::env::temp_dir().join(format!("postbird-attachment-{}.txt", std::process::id()));
        std::fs::write(&path, b"selected file bytes").unwrap();
        let message = ComposeMessage {
            from: None,
            to: "reader@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Images and files".into(),
            body: "Before [Image: Screenshot.png] after".into(),
            html_body: Some(
                "<p>Before<img src=\"cid:screenshot\" alt=\"Screenshot.png\"> after</p>".into(),
            ),
            in_reply_to: None,
            thread_id: None,
            attachments: vec![path.clone()],
            forwarded_attachments: vec![ForwardedAttachment {
                content_id: Some("screenshot".into()),
                filename: "Screenshot.png".into(),
                mime_type: "image/png".into(),
                data: (&b"original image bytes"[..]).into(),
            }],
        };
        let raw = decode_attachment_data(&encode_message("sender@example.com", &message).unwrap())
            .unwrap();
        // Parse independently with the same standard MIME parser used by the IMAP backend.
        let mut parser = Command::new("python3").args(["-c", r#"
import email, email.policy, json, sys
message = email.message_from_bytes(sys.stdin.buffer.read(), policy=email.policy.default)
alternative, file = list(message.iter_parts())
plain, related = list(alternative.iter_parts())
html, image = list(related.iter_parts())
print(json.dumps({
    'structure': [message.get_content_type(), alternative.get_content_type(), related.get_content_type()],
    'plain': plain.get_content().strip(), 'html': html.get_content().strip(),
    'image_id': str(image['Content-ID']), 'image_disposition': image.get_content_disposition(),
    'image_bytes': image.get_payload(decode=True).decode(),
    'file_disposition': file.get_content_disposition(), 'file_bytes': file.get_payload(decode=True).decode()
}))
"#]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        parser.stdin.take().unwrap().write_all(&raw).unwrap();
        let output = parser.wait_with_output().unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(output.status.success());
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            parsed["structure"],
            json!([
                "multipart/mixed",
                "multipart/alternative",
                "multipart/related"
            ])
        );
        assert_eq!(parsed["plain"], message.body);
        assert_eq!(parsed["html"], message.html_body.unwrap());
        assert_eq!(parsed["image_id"], "<screenshot>");
        assert_eq!(parsed["image_disposition"], "inline");
        assert_eq!(parsed["image_bytes"], "original image bytes");
        assert_eq!(parsed["file_disposition"], "attachment");
        assert_eq!(parsed["file_bytes"], "selected file bytes");
    }

    #[test]
    fn forwards_multiple_attachments_in_the_encoded_message() {
        let message = ComposeMessage {
            from: None,
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
    fn content_id_on_body_parts_does_not_hide_formatted_mail() {
        let html = "<html><body><h1>Sale</h1><img src=\"cid:logo\"></body></html>";
        let mut message: Message = serde_json::from_value(json!({
            "id": "marketing", "threadId": "sale", "payload": {
                "mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/plain", "headers": [{"name": "Content-ID", "value": "<plain>"}], "body": {"data": URL_SAFE_NO_PAD.encode("Long tracking URLs")}},
                    {"mimeType": "multipart/related", "headers": [{"name": "Content-ID", "value": "<container>"}], "parts": [
                        {"mimeType": "text/html", "headers": [{"name": "Content-ID", "value": "<body>"}], "body": {"data": URL_SAFE_NO_PAD.encode(html)}},
                        {"mimeType": "image/png", "headers": [{"name": "Content-ID", "value": "<logo>"}], "body": {"data": "aW1hZ2U"}}
                    ]},
                    {"mimeType": "text/html", "headers": [{"name": "Content-Disposition", "value": "attachment"}, {"name": "Content-ID", "value": "<file>"}], "body": {"attachmentId": "2"}},
                    {"mimeType": "application/pdf", "headers": [{"name": "Content-ID", "value": "<pdf>"}], "body": {"attachmentId": "3"}}
                ]
            }
        })).unwrap();
        assert_eq!(message.body_html().as_deref(), Some(html));
        assert_eq!(message.body_text(), "Long tracking URLs");
        let (files, images) = message.attachment_groups();
        assert_eq!(files.len(), 2);
        assert_eq!(images.len(), 1);
        let rendered = message.rendered_body_with_images(&HashMap::new(), false);
        assert!(rendered.contains("<h1>Sale</h1>"));
        assert!(!rendered.contains("Long tracking URLs"));
        assert!(!message.needs_body_refresh());

        let body = &mut message.payload.parts[1].parts[0].body;
        body.data = None;
        body.attachment_id = Some("1.0".into());
        assert!(
            message.needs_body_refresh(),
            "repair previously misclassified cached HTML"
        );
        message.payload.parts[1].parts[0].body.data = Some(URL_SAFE_NO_PAD.encode(html));
        assert!(!message.needs_body_refresh());
        message.payload.parts[1].parts.clear();
        message.payload.parts[1].body.attachment_id = Some("1".into());
        assert!(
            message.needs_body_refresh(),
            "also repair collapsed multipart containers"
        );
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
        assert!(
            html.rendered_body_with_images(&HashMap::new(), false)
                .contains("<b>Hi</b>")
        );

        let plain: Message = serde_json::from_value(json!({
            "id": "2", "threadId": "t", "payload": {
                "mimeType": "text/plain", "body": {"data": "PHNjcmlwdD4"}
            }
        }))
        .unwrap();
        assert!(
            plain
                .rendered_body_with_images(&HashMap::new(), false)
                .contains("&lt;script&gt;")
        );
    }

    #[test]
    fn resolves_local_cid_images_without_enabling_remote_images() {
        let html = r#"<img src="CID:Logo%40example.com"><img src='https://tracker.example/pixel'>"#;
        let message: Message = serde_json::from_value(json!({
            "id": "1", "threadId": "t", "payload": {
                "mimeType": "multipart/related", "parts": [
                    {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode(html)}},
                    {"mimeType": "image/png", "headers": [{"name": "Content-ID", "value": "<Logo@example.com>"}],
                     "body": {"attachmentId": "1"}}
                ]
            }
        }))
        .unwrap();
        assert_eq!(message.inline_image_parts().len(), 1);
        let images = HashMap::from([(
            "logo@example.com".to_owned(),
            "data:image/png;base64,AQID".to_owned(),
        )]);
        let rendered = message.rendered_body_with_images(&images, false);
        assert!(rendered.contains("src=\"data:image/png;base64,AQID\""));
        assert!(rendered.contains("img-src data:;"));
        assert!(rendered.contains("src='https://tracker.example/pixel'"));
        assert!(!rendered.contains("img-src data: http: https:"));
        let remote_enabled = message.rendered_body_with_images(&images, true);
        assert!(remote_enabled.contains("img-src data: http: https:"));
    }

    #[test]
    fn preserves_complete_html_documents() {
        let rendered = render_html_document(
            "<!DOCTYPE html><html><head><style>.email{color:red}</style></head><body class=email>Hi</body></html>".to_owned(),
            false,
        );
        assert_eq!(rendered.matches("<!DOCTYPE").count(), 1);
        assert_eq!(rendered.matches("<html").count(), 1);
        assert!(rendered.contains("postbird-renderer"));
        assert!(rendered.contains(".email{color:red}"));
    }

    #[test]
    fn fixed_size_mail_images_get_aspect_preserving_reader_rules() {
        let rendered = render_html_document(
            "<img width='1200' height='900' style='width:1200px;height:900px' src='cid:photo'>"
                .to_owned(),
            false,
        );
        assert!(rendered.contains("max-width: min(100%, 100vw) !important"));
        assert!(rendered.contains("height: auto !important"));
        assert!(rendered.contains("style='width:1200px;height:900px'"));
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
