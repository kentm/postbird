use std::{cell::Cell, collections::HashSet, rc::Rc};

use adw::prelude::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gtk::{gio, glib};
use webkit6::prelude::*;

use crate::gmail::{ComposeMessage, ForwardedAttachment};

const WORLD: &str = "postbird-compose";

#[derive(Clone)]
pub struct RichEditor {
    pub view: webkit6::WebView,
    ready: Rc<Cell<bool>>,
    images: Vec<ForwardedAttachment>,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    html: String,
    plain: String,
}

impl RichEditor {
    pub fn new(initial: Option<&ComposeMessage>) -> Self {
        let settings = webkit6::Settings::new();
        settings.set_enable_javascript(true);
        settings.set_enable_javascript_markup(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(false);
        let view = webkit6::WebView::builder()
            .settings(&settings)
            .network_session(&webkit6::NetworkSession::new_ephemeral())
            .hexpand(true)
            .vexpand(true)
            .build();
        view.set_background_color(&gtk::gdk::RGBA::WHITE);
        // No navigation, popups, permissions or downloads from message content.
        view.connect_decide_policy(|_, decision, kind| {
            if matches!(
                kind,
                webkit6::PolicyDecisionType::NavigationAction
                    | webkit6::PolicyDecisionType::NewWindowAction
            ) {
                if let Some(nav) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>()
                    && nav
                        .navigation_action()
                        .and_then(|mut a| a.request())
                        .and_then(|r| r.uri())
                        .as_deref()
                        == Some("about:blank")
                {
                    return false;
                }
                decision.ignore();
                return true;
            }
            false
        });
        view.connect_permission_request(|_, request| {
            request.deny();
            true
        });
        install_paste(&view);
        let images = initial
            .into_iter()
            .flat_map(|message| &message.forwarded_attachments)
            .filter(|file| file.content_id.is_some() && file.mime_type.starts_with("image/"))
            .cloned()
            .collect::<Vec<_>>();
        let image_data = images.iter().map(|image| serde_json::json!({
            "cid": image.content_id,
            "src": format!("data:{};base64,{}", image.mime_type, STANDARD.encode(&image.data)),
        })).collect::<Vec<_>>();
        let html = initial
            .map(|message| {
                message
                    .html_body
                    .clone()
                    .unwrap_or_else(|| plain_html(&message.body))
            })
            .unwrap_or_else(|| "<div><br></div>".into());
        // Content is passed as JSON to application-owned code, never as executable markup.
        let script = format!(
            "const INITIAL_HTML = {}; const INITIAL_IMAGES = {};\n{}",
            serde_json::to_string(&html).unwrap(),
            serde_json::to_string(&image_data).unwrap(),
            include_str!("compose_editor.js")
        );
        // Register the world for the document lifetime so callbacks and editor
        // state survive between evaluate_javascript calls.
        view.user_content_manager()
            .unwrap()
            .add_script(&webkit6::UserScript::for_world(
                &script,
                webkit6::UserContentInjectedFrames::TopFrame,
                webkit6::UserScriptInjectionTime::End,
                WORLD,
                &[],
                &[],
            ));
        let ready = Rc::new(Cell::new(false));
        let initialized = ready.clone();
        view.connect_load_changed(move |view, event| {
            if event != webkit6::LoadEvent::Finished {
                return;
            }
            let ready = initialized.clone();
            view.evaluate_javascript(
                "typeof postbirdEditor !== 'undefined'",
                Some(WORLD),
                None,
                None::<&gio::Cancellable>,
                move |result| match result {
                    Ok(value) => ready.set(value.to_boolean()),
                    Err(error) => eprintln!("could not initialize message editor: {error}"),
                },
            );
        });
        view.load_html(r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'">
<style>html { color-scheme:light; background:white; color:#181b20; }
body { min-height:100vh; margin:0; font:14px/1.5 sans-serif; overflow-wrap:anywhere; outline:none; }
img { max-width:100%; height:auto; } blockquote { margin:12px 0; padding-left:14px; border-left:3px solid #ccd1d9; }
</style></head><body></body></html>"#, None);
        Self {
            view,
            ready,
            images,
        }
    }

    pub fn inline_ids(&self) -> HashSet<String> {
        self.images
            .iter()
            .filter_map(|image| image.content_id.clone())
            .collect()
    }

    pub fn toolbar(&self) -> gtk::Box {
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        for (label, tooltip, command) in [
            ("B", "Bold (Ctrl+B)", "Bold"),
            ("I", "Italic (Ctrl+I)", "Italic"),
            ("U", "Underline (Ctrl+U)", "Underline"),
            ("S", "Strikethrough", "Strikethrough"),
            ("• List", "Bulleted list", "InsertUnorderedList"),
            ("1. List", "Numbered list", "InsertOrderedList"),
            ("Clear formatting", "Clear formatting", "RemoveFormat"),
            ("↶", "Undo (Ctrl+Z)", "Undo"),
            ("↷", "Redo (Ctrl+Shift+Z)", "Redo"),
        ] {
            let button = gtk::Button::builder()
                .label(label)
                .tooltip_text(tooltip)
                .css_classes(["flat"])
                .focus_on_click(false)
                .build();
            let view = self.view.downgrade();
            button.connect_clicked(move |_| {
                if let Some(view) = view.upgrade() {
                    view.grab_focus();
                    view.execute_editing_command(command);
                }
            });
            toolbar.append(&button);
        }
        toolbar
    }

    pub async fn content(&self) -> anyhow::Result<(String, String, Vec<ForwardedAttachment>)> {
        anyhow::ensure!(
            self.ready.get(),
            "The message editor is still loading. Please try again."
        );
        let value = self
            .view
            .evaluate_javascript_future("postbirdEditor.snapshot()", Some(WORLD), None)
            .await?;
        let snapshot: Snapshot = serde_json::from_str(&value.to_str())?;
        export(snapshot, &self.images)
    }
}

pub fn plain_html(text: &str) -> String {
    format!(
        "<div style=\"white-space:pre-wrap\">{}</div>",
        glib::markup_escape_text(text)
    )
}

fn export(
    snapshot: Snapshot,
    images: &[ForwardedAttachment],
) -> anyhow::Result<(String, String, Vec<ForwardedAttachment>)> {
    let mut html = snapshot.html;
    let mut attachments = images
        .iter()
        .filter(|image| {
            image
                .content_id
                .as_ref()
                .is_some_and(|id| html.contains(&format!("cid:{}", glib::markup_escape_text(id))))
        })
        .cloned()
        .collect::<Vec<_>>();
    // WebKit pastes bitmap images as data URLs. Send interoperable CID parts,
    // not large data URLs that many email clients cannot display.
    let mut offset = 0;
    while let Some(start) = html[offset..]
        .find("src=\"data:image/")
        .map(|i| i + offset + 5)
    {
        let Some(end) = html[start..].find('"').map(|i| i + start) else {
            break;
        };
        let uri = &html[start..end];
        let (mime, encoded) = uri
            .strip_prefix("data:")
            .unwrap()
            .split_once(";base64,")
            .ok_or_else(|| anyhow::anyhow!("Unsupported pasted image encoding"))?;
        let data = STANDARD.decode(encoded)?;
        let id = format!("postbird-{}@inline", glib::uuid_string_random());
        let extension = match mime {
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            _ => "png",
        };
        attachments.push(ForwardedAttachment {
            content_id: Some(id.clone()),
            filename: format!("Image-{}.{}", attachments.len() + 1, extension),
            mime_type: mime.to_owned(),
            data: data.into(),
        });
        let replacement = format!("cid:{id}");
        html.replace_range(start..end, &replacement);
        offset = start + replacement.len();
    }
    Ok((snapshot.plain, html, attachments))
}

fn install_paste(view: &webkit6::WebView) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = view.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        if (control && matches!(key, gtk::gdk::Key::v | gtk::gdk::Key::V))
            || (shift && key == gtk::gdk::Key::Insert)
        {
            if let Some(view) = weak.upgrade() {
                start_paste(&view, control && shift);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    view.add_controller(keys);
    view.connect_context_menu(|view, menu, _| {
        for item in menu.items() {
            let plain = match item.stock_action() {
                webkit6::ContextMenuAction::Paste => false,
                webkit6::ContextMenuAction::PasteAsPlainText => true,
                _ => continue,
            };
            let action = gio::SimpleAction::new(if plain { "paste-plain" } else { "paste" }, None);
            let weak = view.downgrade();
            action.connect_activate(move |_, _| {
                if let Some(view) = weak.upgrade() {
                    start_paste(&view, plain);
                }
            });
            menu.remove(&item);
            menu.append(&webkit6::ContextMenuItem::from_gaction(
                &action,
                if plain {
                    "Paste as plain text"
                } else {
                    "Paste"
                },
                None,
            ));
        }
        false
    });
}

fn start_paste(view: &webkit6::WebView, plain: bool) {
    let view = view.clone();
    glib::MainContext::default().spawn_local(async move {
        if let Err(error) = paste(&view, plain).await
            && view.root().is_some()
        {
            let alert = adw::AlertDialog::builder()
                .heading("Could not paste")
                .body(error.to_string())
                .build();
            alert.add_response("close", "Close");
            alert.present(Some(&view));
        }
    });
}

async fn paste(view: &webkit6::WebView, plain: bool) -> anyhow::Result<()> {
    let id = view
        .evaluate_javascript_future("postbirdEditor.beginPaste()", Some(WORLD), None)
        .await?
        .to_int32();
    let clipboard = view.clipboard();
    let content: anyhow::Result<String> = async {
        if !plain
            && (clipboard
                .formats()
                .contains_type(gtk::gdk::Texture::static_type())
                || clipboard
                    .formats()
                    .mime_types()
                    .iter()
                    .any(|mime| mime.starts_with("image/")))
        {
            let texture = clipboard
                .read_texture_future()
                .await?
                .ok_or_else(|| anyhow::anyhow!("The clipboard no longer contains an image"))?;
            return Ok(format!(
                "<img src=\"data:image/png;base64,{}\" style=\"max-width:100%;height:auto\">",
                STANDARD.encode(texture.save_to_png_bytes())
            ));
        }
        if !plain && clipboard.formats().contain_mime_type("text/html") {
            let (stream, _) = clipboard
                .read_future(&["text/html"], glib::Priority::DEFAULT)
                .await?;
            let mut bytes = Vec::new();
            loop {
                let chunk = stream
                    .read_bytes_future(65536, glib::Priority::DEFAULT)
                    .await?;
                if chunk.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(String::from_utf8(bytes)?.trim_end_matches('\0').to_owned());
        }
        Ok(plain_html(
            clipboard
                .read_text_future()
                .await?
                .as_deref()
                .unwrap_or_default(),
        ))
    }
    .await;
    // Release the saved selection and pending flag on both success and failure.
    view.evaluate_javascript_future(
        &format!(
            "postbirdEditor.finishPaste({id}, {})",
            serde_json::to_string(&content.as_ref().ok()).unwrap()
        ),
        Some(WORLD),
        None,
    )
    .await?;
    content.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_images_become_mime_parts_and_removed_images_are_omitted() {
        let image = ForwardedAttachment {
            content_id: Some("old".into()),
            filename: "old.png".into(),
            mime_type: "image/png".into(),
            data: vec![1].into(),
        };
        let (plain, html, files) = export(Snapshot {
            plain: "Hello".into(),
            html: "<table><tr><td><b>Hello</b><img src=\"data:image/png;base64,AQID\"></td></tr></table>".into(),
        }, &[image]).unwrap();
        assert_eq!(plain, "Hello");
        assert!(html.contains("<table>"));
        assert!(html.contains("src=\"cid:postbird-"));
        assert!(!html.contains("data:image"));
        assert_eq!(files.len(), 1);
        assert_eq!(&*files[0].data, &[1, 2, 3]);
    }

    #[test]
    #[ignore = "requires an isolated Broadway graphical session"]
    fn html_editing_images_paste_and_draft_round_trip() {
        adw::init().unwrap();
        let context = glib::MainContext::default();
        let message = ComposeMessage {
            from: None, to: "reader@example.com".into(), cc: String::new(), bcc: String::new(),
            subject: "HTML".into(), body: "Original".into(),
            html_body: Some(r#"<html><head><style>.brand { color: rgb(255, 0, 0); }</style></head><body><div><br></div><blockquote><table><tr><td class="brand"><b>Original</b></td></tr></table><a href="https://example.com">Link</a><img src="cid:logo"><img src="https://example.invalid/tracker"><script>window.bad=true</script><img src="bad" onerror="window.bad=true"></blockquote></body></html>"#.into()),
            in_reply_to: Some("<original@example.com>".into()), thread_id: Some("thread".into()), attachments: Vec::new(),
            forwarded_attachments: vec![ForwardedAttachment {
                content_id: Some("logo".into()), filename: "logo.png".into(), mime_type: "image/png".into(),
                data: STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLttAAAAABJRU5ErkJggg==").unwrap().into(),
            }],
        };
        let editor = RichEditor::new(Some(&message));
        let window = gtk::Window::builder()
            .default_width(900)
            .default_height(600)
            .child(&editor.view)
            .build();
        window.present();
        let wait = |ready: &Cell<bool>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while !ready.get() && std::time::Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(ready.get(), "HTML editor did not initialize");
        };
        wait(&editor.ready);
        let js = |code: &str| {
            context
                .block_on(
                    editor
                        .view
                        .evaluate_javascript_future(code, Some(WORLD), None),
                )
                .unwrap()
        };
        assert_eq!(
            js("getComputedStyle(document.querySelector('.brand')).color").to_str(),
            "rgb(255, 0, 0)"
        );
        assert!(js("document.querySelector('img').naturalWidth === 1").to_boolean());
        assert!(js("!document.querySelector('script') && !document.querySelector('[onerror]') && !window.bad").to_boolean());
        editor.view.grab_focus();
        js(
            "document.body.focus(); const range = document.createRange(); range.selectNodeContents(document.body.querySelector('div')); range.collapse(true); getSelection().removeAllRanges(); getSelection().addRange(range); document.execCommand('insertText', false, 'My reply');",
        );
        let (plain, html, images) = context.block_on(editor.content()).unwrap();
        assert!(plain.contains("My reply") && plain.contains("Original"));
        assert!(html.contains("<table>") && html.contains("<b>Original</b>"));
        assert!(html.contains("https://example.com"));
        assert!(html.contains("cid:logo"));
        assert_eq!(images.len(), 1);
        js("document.execCommand('undo')");
        assert!(
            !context
                .block_on(editor.content())
                .unwrap()
                .0
                .contains("My reply")
        );
        js("document.execCommand('redo')");
        assert!(
            context
                .block_on(editor.content())
                .unwrap()
                .0
                .contains("My reply")
        );
        js(
            "const selection = document.createRange(); selection.selectNodeContents(document.body.querySelector('div')); getSelection().removeAllRanges(); getSelection().addRange(selection);",
        );
        editor.view.execute_editing_command("Bold");
        let formatted = context.block_on(editor.content()).unwrap().1;
        assert!(formatted.contains("<b>My reply</b>"), "{formatted}");
        js("document.execCommand('undo'); getSelection().collapseToEnd();");
        // Exercise HTML clipboard sanitizing and preserve formatting on paste.
        js(
            "const transfer = new DataTransfer(); transfer.setData('text/html', '<p><i>Pasted HTML</i><script>window.bad=true</script></p>'); document.body.dispatchEvent(new ClipboardEvent('paste', {clipboardData:transfer, bubbles:true, cancelable:true}));",
        );
        assert!(
            context
                .block_on(editor.content())
                .unwrap()
                .1
                .contains("<i>Pasted HTML</i>")
        );
        // Native bitmap paste must survive as a MIME inline part, including undo/redo.
        let pixbuf =
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 20, 10)
                .unwrap();
        pixbuf.fill(0x315c9eff);
        editor
            .view
            .clipboard()
            .set_texture(&gtk::gdk::Texture::for_pixbuf(&pixbuf));
        context.block_on(paste(&editor.view, false)).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            context.iteration(false);
            if context
                .block_on(editor.content())
                .is_ok_and(|c| c.2.len() == 2)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (_, _, pasted) = context.block_on(editor.content()).unwrap();
        assert_eq!(
            pasted.len(),
            2,
            "native clipboard bitmap should be embedded"
        );
        js("document.execCommand('undo')");
        assert_eq!(context.block_on(editor.content()).unwrap().2.len(), 1);
        js("document.execCommand('redo')");
        assert_eq!(context.block_on(editor.content()).unwrap().2.len(), 2);
        let (plain, html, images) = context.block_on(editor.content()).unwrap();
        let draft = ComposeMessage {
            body: plain.clone(),
            html_body: Some(html),
            forwarded_attachments: images,
            ..message
        };
        let reopened = RichEditor::new(Some(&draft));
        window.set_child(Some(&reopened.view));
        wait(&reopened.ready);
        let restored = context.block_on(reopened.content()).unwrap();
        assert_eq!(restored.0, plain);
        assert!(restored.1.contains("<table>"));
        assert_eq!(restored.2.len(), 2);
        window.destroy();
    }
}
