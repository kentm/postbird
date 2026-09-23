use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use html2text::render::{RichAnnotation, TaggedLineElement};

use crate::gmail::{ComposeMessage, ForwardedAttachment};

#[derive(Clone)]
enum ComposeAttachment {
    File(PathBuf),
    Data(ForwardedAttachment),
}

impl ComposeAttachment {
    fn filename(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Self::Data(file) => file.filename.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AttachmentList {
    pub widget: gtk::ScrolledWindow,
    rows: gtk::Box,
    items: Rc<RefCell<Vec<ComposeAttachment>>>,
}

impl AttachmentList {
    pub fn new(initial: Option<&ComposeMessage>, restored_images: &HashSet<String>) -> Self {
        let mut items = Vec::new();
        if let Some(initial) = initial {
            items.extend(
                initial
                    .attachments
                    .iter()
                    .cloned()
                    .map(ComposeAttachment::File),
            );
            items.extend(
                initial
                    .forwarded_attachments
                    .iter()
                    .filter(|file| {
                        !file
                            .content_id
                            .as_ref()
                            .is_some_and(|id| restored_images.contains(id))
                    })
                    .cloned()
                    .map(ComposeAttachment::Data),
            );
        }
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .max_content_height(130)
            .propagate_natural_height(true)
            .child(&rows)
            .build();
        let list = Self {
            widget,
            rows,
            items: Rc::new(RefCell::new(items)),
        };
        list.refresh();
        list
    }

    pub fn add_files(&self, files: &gio::ListModel) -> anyhow::Result<()> {
        // Validate the whole selection before adding any of it.
        let mut paths = Vec::new();
        for index in 0..files.n_items() {
            let file = files
                .item(index)
                .and_downcast::<gio::File>()
                .ok_or_else(|| anyhow::anyhow!("The selected file is unavailable"))?;
            let path = file
                .path()
                .ok_or_else(|| anyhow::anyhow!("Choose a local file to attach"))?;
            anyhow::ensure!(path.is_file(), "{} is not a file", path.display());
            paths.push(path);
        }
        let mut items = self.items.borrow_mut();
        for path in paths {
            if !items
                .iter()
                .any(|item| matches!(item, ComposeAttachment::File(existing) if existing == &path))
            {
                items.push(ComposeAttachment::File(path));
            }
        }
        drop(items);
        self.refresh();
        Ok(())
    }

    pub fn contents(&self) -> (Vec<PathBuf>, Vec<ForwardedAttachment>) {
        let mut paths = Vec::new();
        let mut data = Vec::new();
        for item in self.items.borrow().iter() {
            match item {
                ComposeAttachment::File(path) => paths.push(path.clone()),
                ComposeAttachment::Data(file) => data.push(file.clone()),
            }
        }
        (paths, data)
    }

    fn refresh(&self) {
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        self.widget.set_visible(!self.items.borrow().is_empty());
        for (index, item) in self.items.borrow().iter().enumerate() {
            let filename = item.filename();
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.append(&gtk::Image::from_icon_name("postbird-paperclip-symbolic"));
            row.append(
                &gtk::Label::builder()
                    .label(&filename)
                    .tooltip_text(&filename)
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            );
            let remove = gtk::Button::builder()
                .icon_name("postbird-circle-x-symbolic")
                .tooltip_text(format!("Remove {filename}"))
                .css_classes(["flat"])
                .build();
            remove.update_property(&[gtk::accessible::Property::Label(&format!(
                "Remove {filename}"
            ))]);
            let items = self.items.clone();
            let rows = self.rows.downgrade();
            let widget = self.widget.downgrade();
            remove.connect_clicked(move |_| {
                let (Some(rows), Some(widget)) = (rows.upgrade(), widget.upgrade()) else {
                    return;
                };
                items.borrow_mut().remove(index);
                Self {
                    rows,
                    widget,
                    items: items.clone(),
                }
                .refresh();
            });
            row.append(&remove);
            self.rows.append(&row);
        }
    }
}

#[derive(Clone)]
struct InlineImage {
    preview: gdk::Texture,
    attachment: ForwardedAttachment,
}

#[derive(Clone, Default)]
pub struct InlineImages {
    images: Rc<RefCell<Vec<InlineImage>>>,
    pending: Rc<Cell<usize>>,
}

impl InlineImages {
    pub fn is_pending(&self) -> bool {
        self.pending.get() > 0
    }

    pub fn at(&self, iter: &gtk::TextIter) -> Option<ForwardedAttachment> {
        let paintable = iter.paintable()?;
        self.images
            .borrow()
            .iter()
            .find(|image| image.preview.upcast_ref::<gdk::Paintable>() == &paintable)
            .map(|image| image.attachment.clone())
    }

    pub fn attachments(&self, editor: &gtk::TextView) -> Vec<ForwardedAttachment> {
        let buffer = editor.buffer();
        let mut iter = buffer.start_iter();
        let mut found = HashSet::new();
        let mut images = Vec::new();
        while iter.offset() < buffer.end_iter().offset() {
            if let Some(image) = self.at(&iter)
                && found.insert(image.content_id.clone())
            {
                images.push(image);
            }
            iter.forward_char();
        }
        images
    }

    fn insert_attachment(
        &self,
        buffer: &gtk::TextBuffer,
        iter: &mut gtk::TextIter,
        attachment: ForwardedAttachment,
    ) -> anyhow::Result<()> {
        if let Some(image) = self
            .images
            .borrow()
            .iter()
            .find(|image| image.attachment.content_id == attachment.content_id)
        {
            buffer.insert_paintable(iter, &image.preview);
            return Ok(());
        }
        let pixbuf =
            gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(attachment.data.clone()))?;
        let scale = (520.0 / f64::from(pixbuf.width()))
            .min(320.0 / f64::from(pixbuf.height()))
            .min(1.0);
        let preview = if scale < 1.0 {
            pixbuf
                .scale_simple(
                    (f64::from(pixbuf.width()) * scale).round().max(1.0) as i32,
                    (f64::from(pixbuf.height()) * scale).round().max(1.0) as i32,
                    gtk::gdk_pixbuf::InterpType::Bilinear,
                )
                .ok_or_else(|| anyhow::anyhow!("Could not resize image preview"))?
        } else {
            pixbuf
        };
        let preview = gdk::Texture::for_pixbuf(&preview);
        self.images.borrow_mut().push(InlineImage {
            preview: preview.clone(),
            attachment,
        });
        buffer.insert_paintable(iter, &preview);
        Ok(())
    }

    pub fn connect_paste(&self, editor: &gtk::TextView, dialog: &adw::Dialog) {
        crate::compose_history::install(editor);
        let images = self.clone();
        let dialog = dialog.downgrade();
        editor.connect_paste_clipboard(move |editor| {
            let clipboard = editor.clipboard();
            let formats = clipboard.formats();
            if !formats.contains_type(gdk::Texture::static_type())
                && !formats
                    .mime_types()
                    .iter()
                    .any(|mime| mime.starts_with("image/"))
            {
                return;
            }
            editor.stop_signal_emission_by_name("paste-clipboard");
            if !editor.is_editable() || !editor.is_sensitive() {
                return;
            }
            let buffer = editor.buffer();
            let (start, end) = buffer.selection_bounds().unwrap_or_else(|| {
                let cursor = buffer.iter_at_offset(buffer.cursor_position());
                (cursor, cursor)
            });
            let start = buffer.create_mark(None, &start, true);
            let end = buffer.create_mark(None, &end, true);
            let editor = editor.downgrade();
            let images = images.clone();
            let dialog = dialog.clone();
            images.pending.set(images.pending.get() + 1);
            glib::MainContext::default().spawn_local(async move {
                let result = clipboard.read_texture_future().await;
                images.pending.set(images.pending.get() - 1);
                if let Some(editor) = editor.upgrade().filter(|editor| editor.root().is_some()) {
                    let result = match result {
                        Ok(Some(texture)) => {
                            let attachment = ForwardedAttachment {
                                content_id: Some(format!(
                                    "postbird-{}@inline",
                                    glib::uuid_string_random()
                                )),
                                filename: format!(
                                    "Screenshot-{}.png",
                                    images.images.borrow().len() + 1
                                ),
                                mime_type: "image/png".to_owned(),
                                data: texture.save_to_png_bytes().as_ref().into(),
                            };
                            buffer.begin_user_action();
                            let mut to = buffer.iter_at_mark(&end);
                            // Insert successfully before replacing the selected text.
                            let result = images.insert_attachment(&buffer, &mut to, attachment);
                            if result.is_ok() {
                                let mut selected_end = buffer.iter_at_mark(&end);
                                let mut from = buffer.iter_at_mark(&start);
                                buffer.delete(&mut from, &mut selected_end);
                                from.forward_char();
                                buffer.place_cursor(&from);
                                editor.scroll_mark_onscreen(&buffer.get_insert());
                            }
                            buffer.end_user_action();
                            result
                        }
                        Ok(None) => {
                            Err(anyhow::anyhow!("The clipboard no longer contains an image"))
                        }
                        Err(error) => Err(error.into()),
                    };
                    if let Err(error) = result
                        && let Some(dialog) = dialog.upgrade()
                    {
                        show_error(&dialog, error);
                    }
                }
                buffer.delete_mark(&start);
                buffer.delete_mark(&end);
            });
        });
    }

    pub fn restore(&self, editor: &gtk::TextView, message: &ComposeMessage) -> HashSet<String> {
        let buffer = editor.buffer();
        buffer.set_text(&message.body);
        let mut restored = HashSet::new();
        let Some(html) = message.html_body.as_deref().filter(|_| {
            message
                .forwarded_attachments
                .iter()
                .any(|file| file.content_id.is_some())
        }) else {
            return restored;
        };
        let Ok(lines) = html2text::config::rich()
            .empty_img_mode(html2text::config::ImageRenderMode::Replace("\u{fffc}"))
            .lines_from_read(html.as_bytes(), 4096)
        else {
            return restored;
        };
        buffer.set_text("");
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                buffer.insert(&mut buffer.end_iter(), "\n");
            }
            for element in line.iter() {
                let TaggedLineElement::Str(text) = element else {
                    continue;
                };
                let image = text.tag.iter().find_map(|tag| {
                    let RichAnnotation::Image(src) = tag else {
                        return None;
                    };
                    let id = src.strip_prefix("cid:")?;
                    message
                        .forwarded_attachments
                        .iter()
                        .find(|file| file.content_id.as_deref() == Some(id))
                });
                if let Some(image) = image
                    && self
                        .insert_attachment(&buffer, &mut buffer.end_iter(), image.clone())
                        .is_ok()
                {
                    restored.insert(image.content_id.clone().unwrap());
                    continue;
                }
                let start = buffer.end_iter().offset();
                buffer.insert(&mut buffer.end_iter(), &text.s);
                for tag in &text.tag {
                    let name = match tag {
                        RichAnnotation::Strong => "postbird-bold",
                        RichAnnotation::Emphasis => "postbird-italic",
                        RichAnnotation::Strikeout => "postbird-strike",
                        _ => continue,
                    };
                    buffer.apply_tag_by_name(
                        name,
                        &buffer.iter_at_offset(start),
                        &buffer.end_iter(),
                    );
                }
            }
        }
        restored
    }
}

pub fn show_error(parent: &adw::Dialog, error: impl std::fmt::Display) {
    let alert = adw::AlertDialog::builder()
        .heading("Postbird could not complete that action")
        .body(error.to_string())
        .build();
    alert.add_response("close", "Close");
    alert.present(Some(parent));
}
