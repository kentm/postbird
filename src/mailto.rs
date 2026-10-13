use anyhow::{Context, Result, ensure};

use crate::gmail::ComposeMessage;

pub fn is_mailto(uri: &str) -> bool {
    uri.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("mailto"))
}

/// Decode only the fields that the composer can safely show for review.
/// In particular, a URI cannot select a sender, add attachments, or send mail.
pub fn parse(uri: &str) -> Result<ComposeMessage> {
    ensure!(is_mailto(uri), "Postbird can only open mailto: links");
    ensure!(!uri.contains('#'), "Encode # as %23 in mailto: links");
    let (_, content) = uri.split_once(':').unwrap();
    // GDaemonVfs represents opaque mailto URIs as mailto:/// through GFile.
    // Remove only that synthetic prefix, before decoding: %2F in a recipient
    // is part of its address and must not be stripped.
    let content = content.strip_prefix("///").unwrap_or(content);
    let (to, query) = content.split_once('?').unwrap_or((content, ""));
    let mut message = ComposeMessage {
        from: None,
        to: decode(to, false)?,
        cc: String::new(),
        bcc: String::new(),
        subject: String::new(),
        body: String::new(),
        html_body: None,
        in_reply_to: None,
        thread_id: None,
        attachments: Vec::new(),
        forwarded_attachments: Vec::new(),
    };
    let mut has_subject = false;
    let mut has_body = false;
    for field in query.split('&').filter(|field| !field.is_empty()) {
        let (name, value) = field
            .split_once('=')
            .context("A mailto: field is missing its value")?;
        let name = decode(name, false)?.to_ascii_lowercase();
        match name.as_str() {
            "to" | "cc" | "bcc" => {
                let value = decode(value, false)?;
                let recipients = match name.as_str() {
                    "cc" => &mut message.cc,
                    "bcc" => &mut message.bcc,
                    _ => &mut message.to,
                };
                if !value.is_empty() {
                    if !recipients.is_empty() {
                        recipients.push_str(", ");
                    }
                    recipients.push_str(&value);
                }
            }
            "subject" => {
                ensure!(!has_subject, "A mailto: link has more than one subject");
                has_subject = true;
                message.subject = decode(value, false)?;
            }
            "body" => {
                ensure!(!has_body, "A mailto: link has more than one body");
                has_body = true;
                message.body = decode(value, true)?
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
            }
            _ => {} // Ignore unsupported headers, including from and attach.
        }
    }
    Ok(message)
}

fn decode(value: &str, body: bool) -> Result<String> {
    // Unlike form decoding, '+' is literal. Decode exactly once, as RFC 6068
    // requires, and reject malformed escapes and non-UTF-8 text.
    let bytes = gtk::glib::Uri::unescape_bytes(value, None)
        .context("Invalid percent encoding in mailto: link")?;
    let text =
        String::from_utf8(bytes.to_vec()).context("A mailto: link contains invalid UTF-8")?;
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && !(body && matches!(c, '\r' | '\n' | '\t'))),
        "A mailto: link contains control characters in a header or body"
    );
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT_LINK: &str = "mailto:hello@shipsite.co?subject=Report%20hosted%20site%3A%20elderly-plum-qgkb&body=Site%3A%20https%3A%2F%2Felderly-plum-qgkb.shipsite.co%0AReason%3A%20";

    fn assert_report_message(uri: &str) {
        let message = parse(uri).unwrap();
        assert_eq!(message.to, "hello@shipsite.co");
        assert_eq!(message.subject, "Report hosted site: elderly-plum-qgkb");
        assert_eq!(
            message.body,
            "Site: https://elderly-plum-qgkb.shipsite.co\nReason: "
        );
    }

    #[test]
    fn desktop_file_normalization_does_not_add_slashes_to_recipients() {
        assert_report_message(REPORT_LINK);
        assert_report_message(&REPORT_LINK.replacen("mailto:", "mailto:///", 1));

        // Exercise the same GFile conversion as GApplication's open signal.
        // GLocalVfs preserves the URI; the desktop's GDaemonVfs adds ///.
        use gtk::gio::prelude::*;
        assert_report_message(gtk::gio::File::for_uri(REPORT_LINK).uri().as_str());
        assert_report_message(
            gtk::gio::File::for_commandline_arg(REPORT_LINK)
                .uri()
                .as_str(),
        );
    }

    #[test]
    fn desktop_file_prefix_preserves_empty_recipients_and_encoded_slashes() {
        assert!(parse("mailto:///").unwrap().to.is_empty());
        let message = parse("mailto:///?subject=Hello&body=https%3A%2F%2Fexample.com").unwrap();
        assert!(message.to.is_empty());
        assert_eq!(message.subject, "Hello");
        assert_eq!(message.body, "https://example.com");
        for uri in [
            "mailto:%2Fhello@example.com",
            "mailto:///%2Fhello@example.com",
        ] {
            assert_eq!(parse(uri).unwrap().to, "/hello@example.com");
        }
    }

    #[test]
    fn decodes_recipients_headers_and_plain_body() {
        let message = parse("MAILTO:alice+tag@example.com,bob@example.com?to=extra%40example.com&CC=copy%40example.com&cc=copy2@example.com&Bcc=hidden@example.com&sub%6Aect=Caf%C3%A9%20%26%20tea+cake&body=Hello%0D%0A%3Cb%3Eworld%3C%2Fb%3E%20%2520").unwrap();
        assert_eq!(
            message.to,
            "alice+tag@example.com,bob@example.com, extra@example.com"
        );
        assert_eq!(message.cc, "copy@example.com, copy2@example.com");
        assert_eq!(message.bcc, "hidden@example.com");
        assert_eq!(message.subject, "Café & tea+cake");
        assert_eq!(message.body, "Hello\n<b>world</b> %20");
        assert!(message.html_body.is_none());
    }

    #[test]
    fn accepts_blank_and_query_only_links() {
        assert!(parse("mailto:").unwrap().to.is_empty());
        let message = parse("mailto:?to=one@example.com&subject=Hello%3F%23%3D").unwrap();
        assert_eq!(message.to, "one@example.com");
        assert_eq!(message.subject, "Hello?#=");
    }

    #[test]
    fn ignores_sender_attachments_and_other_headers() {
        let message = parse("mailto:a@example.com?from=spoof@example.com&attach=/etc/passwd&attachment=file:///tmp/file&in-reply-to=id&content-type=text/html&x-custom=value").unwrap();
        assert!(message.from.is_none());
        assert!(message.attachments.is_empty());
        assert!(message.forwarded_attachments.is_empty());
        assert!(message.in_reply_to.is_none());
        assert!(message.html_body.is_none());
    }

    #[test]
    fn rejects_malformed_or_ambiguous_links_and_header_injection() {
        for uri in [
            "https://example.com",
            "file:///tmp/message",
            "mailto:a@example.com#fragment",
            "mailto:?subject=%",
            "mailto:?subject=%XY",
            "mailto:?subject=%FF",
            "mailto:?subject=%00",
            "mailto:?body=%00",
            "mailto:?subject=one%0D%0ABcc:evil@example.com",
            "mailto:a@example.com%0Acc:evil@example.com",
            "mailto:?cc=copy@example.com%0D",
            "mailto:?subject=one&SUBJECT=two",
            "mailto:?body=one&body=two",
            "mailto:?subject",
        ] {
            assert!(parse(uri).is_err(), "accepted {uri}");
        }
    }
}
