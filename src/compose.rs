use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc};

use adw::prelude::*;
use gtk::gio;

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

pub fn show_error(parent: &adw::Dialog, error: impl std::fmt::Display) {
    let alert = adw::AlertDialog::builder()
        .heading("Postbird could not complete that action")
        .body(error.to_string())
        .build();
    alert.add_response("close", "Close");
    alert.present(Some(parent));
}
