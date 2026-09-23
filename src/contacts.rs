//! Recipient suggestions are local to the selected account, populated by cache sync.
use adw::prelude::*;
use gtk::{gio, glib};
use lettre::message::{Mailbox, Mailboxes};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub email: String,
    pub name: String,
}

impl Contact {
    pub fn address(&self) -> String {
        Mailbox::new(
            (!self.name.is_empty()).then(|| self.name.clone()),
            self.email.parse().expect("indexed address was validated"),
        )
        .to_string()
    }
}

pub fn from_message(message: &crate::gmail::Message) -> Vec<Contact> {
    ["From", "Reply-To", "To", "Cc"]
        .into_iter()
        .flat_map(|header| {
            message
                .header(header)
                .parse::<Mailboxes>()
                .ok()
                .into_iter()
                .flatten()
        })
        .map(|mailbox| Contact {
            email: mailbox.email.to_string().to_lowercase(),
            name: mailbox.name.unwrap_or_default(),
        })
        .collect()
}

// Byte boundaries of the address containing the caret. A quoted display name
// can contain commas, and completing an earlier address must preserve later ones.
fn recipient_span(text: &str, caret: usize) -> (usize, usize) {
    let mut quoted = false;
    let mut escaped = false;
    let mut angle = false;
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                if index >= caret {
                    return (start, index);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    (start, text.len())
}

fn entry_span(entry: &gtk::Entry) -> (String, usize, usize) {
    let text = entry.text().to_string();
    let caret = text
        .char_indices()
        .nth(entry.position().max(0) as usize)
        .map_or(text.len(), |(index, _)| index);
    let (start, end) = recipient_span(&text, caret);
    (text, start, end)
}

// Own allocation of the entry and its popup: the anchor always covers the full
// input, and GTK can reposition the popup when the dialog is resized.
mod field_imp {
    use gtk::{glib, prelude::*, subclass::prelude::*};
    use std::cell::OnceCell;

    #[derive(Default)]
    pub struct RecipientField {
        pub entry: OnceCell<gtk::Entry>,
        pub popup: OnceCell<gtk::Popover>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RecipientField {
        const NAME: &'static str = "PostbirdRecipientField";
        type Type = super::RecipientField;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for RecipientField {
        fn dispose(&self) {
            if let Some(popup) = self.popup.get() {
                popup.unparent();
            }
            if let Some(entry) = self.entry.get() {
                entry.unparent();
            }
        }
    }

    impl WidgetImpl for RecipientField {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.entry.get().unwrap().measure(orientation, for_size)
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.entry
                .get()
                .unwrap()
                .allocate(width, height, baseline, None);
            let popup = self.popup.get().unwrap();
            popup.set_pointing_to(Some(&gtk::gdk::Rectangle::new(0, 0, width, height)));
            popup.set_size_request(width, -1);
            popup.present();
        }
    }
}

glib::wrapper! {
    pub struct RecipientField(ObjectSubclass<field_imp::RecipientField>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

use gtk::subclass::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

struct Suggestions {
    entry: glib::WeakRef<gtk::Entry>,
    popup: glib::WeakRef<gtk::Popover>,
    list: glib::WeakRef<gtk::ListBox>,
    focus: glib::WeakRef<gtk::EventControllerFocus>,
    contacts: Rc<RefCell<Vec<Contact>>>,
    matches: RefCell<Vec<String>>,
    queued: Cell<bool>,
    updating: Cell<bool>,
    dismissed: Cell<bool>,
}

impl Suggestions {
    fn queue(self: &Rc<Self>) {
        if self.queued.replace(true) {
            return;
        }
        let this = self.clone();
        glib::idle_add_local_once(move || {
            this.queued.set(false);
            this.refresh();
        });
    }

    fn refresh(&self) {
        let (Some(entry), Some(popup), Some(list)) = (
            self.entry.upgrade(),
            self.popup.upgrade(),
            self.list.upgrade(),
        ) else {
            return;
        };
        if self.dismissed.get()
            || !self
                .focus
                .upgrade()
                .is_some_and(|focus| focus.contains_focus())
        {
            popup.popdown();
            return;
        }
        let (text, start, end) = entry_span(&entry);
        let query = text[start..end].trim().to_lowercase();
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let mut matches = self.matches.borrow_mut();
        matches.clear();
        if !query.is_empty() {
            for contact in self
                .contacts
                .borrow()
                .iter()
                .filter(|contact| {
                    contact.name.to_lowercase().contains(&query)
                        || contact.email.to_lowercase().contains(&query)
                })
                .take(8)
            {
                let address = contact.address();
                if address.to_lowercase() == query {
                    continue;
                }
                let details = gtk::Box::new(gtk::Orientation::Vertical, 2);
                let name = gtk::Label::builder()
                    .label(if contact.name.is_empty() {
                        &contact.email
                    } else {
                        &contact.name
                    })
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                details.append(&name);
                if !contact.name.is_empty() {
                    details.append(
                        &gtk::Label::builder()
                            .label(&contact.email)
                            .xalign(0.0)
                            .ellipsize(gtk::pango::EllipsizeMode::End)
                            .css_classes(["postbird-contact-email"])
                            .build(),
                    );
                }
                let row = gtk::ListBoxRow::builder()
                    .child(&details)
                    .focusable(false)
                    .focus_on_click(false)
                    .build();
                row.update_property(&[gtk::accessible::Property::Label(&address)]);
                list.append(&row);
                matches.push(address);
            }
        }
        if matches.is_empty() {
            popup.popdown();
        } else {
            // Keep typing focus in the entry. Arrow keys choose a row; Enter
            // accepts it, while Escape and Tab dismiss without changing text.
            list.select_row(list.row_at_index(0).as_ref());
            popup.popup();
        }
    }

    fn accept(&self, index: i32) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        let Some(address) = self.matches.borrow().get(index as usize).cloned() else {
            return;
        };
        let (text, start, end) = entry_span(&entry);
        let replacement = format!(
            "{}{}{address}",
            &text[..start],
            if start > 0 { " " } else { "" }
        );
        self.updating.set(true);
        entry.set_text(&format!("{replacement}{}", &text[end..]));
        entry.set_position(replacement.chars().count() as i32);
        self.updating.set(false);
        self.dismiss();
        entry.grab_focus();
    }

    fn dismiss(&self) {
        self.dismissed.set(true);
        if let Some(popup) = self.popup.upgrade() {
            popup.popdown();
        }
    }

    fn key(&self, key: gtk::gdk::Key) -> glib::Propagation {
        let (Some(popup), Some(list)) = (self.popup.upgrade(), self.list.upgrade()) else {
            return glib::Propagation::Proceed;
        };
        if !popup.is_visible() {
            return glib::Propagation::Proceed;
        }
        match key {
            gtk::gdk::Key::Down | gtk::gdk::Key::Up => {
                let current = list.selected_row().map_or(0, |row| row.index());
                let delta = if key == gtk::gdk::Key::Down { 1 } else { -1 };
                let next = (current + delta).rem_euclid(self.matches.borrow().len().max(1) as i32);
                if let Some(row) = list.row_at_index(next) {
                    list.select_row(Some(&row));
                    if let Some(scroll) = list
                        .ancestor(gtk::ScrolledWindow::static_type())
                        .and_downcast::<gtk::ScrolledWindow>()
                        && let Some(bounds) = row.compute_bounds(&list)
                    {
                        let adjustment = scroll.vadjustment();
                        let top = f64::from(bounds.y());
                        let bottom = top + f64::from(bounds.height());
                        if top < adjustment.value() {
                            adjustment.set_value(top);
                        } else if bottom > adjustment.value() + adjustment.page_size() {
                            adjustment.set_value(bottom - adjustment.page_size());
                        }
                    }
                }
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                if let Some(row) = list.selected_row() {
                    self.accept(row.index());
                }
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Escape => {
                self.dismiss();
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Tab | gtk::gdk::Key::ISO_Left_Tab => {
                self.dismiss();
                glib::Propagation::Proceed
            }
            _ => glib::Propagation::Proceed,
        }
    }
}

fn field(
    entry: &gtk::Entry,
    contacts: Rc<RefCell<Vec<Contact>>>,
) -> (RecipientField, Rc<Suggestions>) {
    let field: RecipientField = glib::Object::new();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .focusable(false)
        .focus_on_click(false)
        .css_classes(["postbird-contact-list"])
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(280)
        .propagate_natural_height(true)
        .build();
    let popup = gtk::Popover::builder()
        .child(&scroll)
        .position(gtk::PositionType::Bottom)
        .has_arrow(false)
        .autohide(false)
        .focusable(false)
        .css_classes(["postbird-contact-popup"])
        .build();
    popup.set_offset(0, 6);
    entry.set_parent(&field);
    popup.set_parent(&field);
    field.imp().entry.set(entry.clone()).unwrap();
    field.imp().popup.set(popup.clone()).unwrap();
    let focus = gtk::EventControllerFocus::new();
    let state = Rc::new(Suggestions {
        entry: entry.downgrade(),
        popup: popup.downgrade(),
        list: list.downgrade(),
        focus: focus.downgrade(),
        contacts,
        matches: RefCell::new(Vec::new()),
        queued: Cell::new(false),
        updating: Cell::new(false),
        dismissed: Cell::new(false),
    });
    let changed = state.clone();
    entry.connect_changed(move |_| {
        if !changed.updating.get() {
            changed.dismissed.set(false);
            changed.queue();
        }
    });
    let cursor = state.clone();
    entry.connect_cursor_position_notify(move |_| cursor.queue());
    let entering = state.clone();
    focus.connect_enter(move |_| {
        entering.dismissed.set(false);
        entering.queue();
    });
    let leaving = state.clone();
    focus.connect_leave(move |_| leaving.dismiss());
    entry.add_controller(focus);
    let choosing = state.clone();
    list.connect_row_activated(move |_, row| choosing.accept(row.index()));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let navigating = state.clone();
    keys.connect_key_pressed(move |_, key, _, _| navigating.key(key));
    entry.add_controller(keys);
    (field, state)
}

pub fn install(entries: &[gtk::Entry], account: &str) -> Vec<RecipientField> {
    let contacts = Rc::new(RefCell::new(Vec::new()));
    let (fields, states): (Vec<_>, Vec<_>) = entries
        .iter()
        .map(|entry| field(entry, contacts.clone()))
        .unzip();
    let account = account.to_owned();
    glib::MainContext::default().spawn_local(async move {
        let result = gio::spawn_blocking(move || {
            let mut cache = crate::cache::MailCache::open()?;
            cache.index_cached_contacts(&account)?;
            cache.contacts(&account)
        })
        .await;
        match result {
            Ok(Ok(found)) => {
                *contacts.borrow_mut() = found;
                for state in states {
                    state.queue();
                }
            }
            result => eprintln!("could not load recipient suggestions: {result:?}"),
        }
    });
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn suggestions_fill_all_recipient_fields_without_replacing_neighbors() {
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        crate::ui::install_visual_style();
        let contacts = Rc::new(RefCell::new(vec![
            Contact {
                name: "Jane Doe".into(),
                email: "jane@example.com".into(),
            },
            Contact {
                name: "James Smith".into(),
                email: "james@example.com".into(),
            },
        ]));
        let entries = [gtk::Entry::new(), gtk::Entry::new(), gtk::Entry::new()];
        let container = gtk::Box::new(gtk::Orientation::Vertical, 8);
        container.set_margin_top(16);
        container.set_margin_start(16);
        container.set_margin_end(16);
        let mut fields = Vec::new();
        let mut states = Vec::new();
        for (entry, placeholder) in entries.iter().zip(["To", "Cc", "Bcc"]) {
            entry.set_placeholder_text(Some(placeholder));
            let (field, state) = field(entry, contacts.clone());
            container.append(&field);
            fields.push(field);
            states.push(state);
        }
        container.append(&gtk::Entry::builder().placeholder_text("Subject").build());
        let dialog = adw::Dialog::builder()
            .content_width(800)
            .content_height(500)
            .child(&container)
            .build();
        dialog.add_css_class("postbird-compose");
        let window = adw::ApplicationWindow::builder()
            .default_width(1000)
            .default_height(800)
            .build();
        window.present();
        dialog.present(Some(&window));
        let context = glib::MainContext::default();
        let wait = |condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !condition() && std::time::Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(condition(), "suggestions did not reach the expected state");
        };
        wait(&|| entries[0].is_mapped());
        for (index, (entry, state)) in entries.iter().zip(&states).enumerate() {
            entry.grab_focus();
            let text = "\"Other, Person\" <other@example.com>, ja, last@example.com";
            entry.set_text(text);
            entry.set_position((text.find("ja,").unwrap() + 2) as i32);
            let popup = state.popup.upgrade().unwrap();
            let list = state.list.upgrade().unwrap();
            wait(&|| popup.is_mapped() && list.height() > 0);
            assert_eq!(state.matches.borrow().len(), 2);
            let row = list.row_at_index(0).unwrap();
            let labels = row.child().unwrap().downcast::<gtk::Box>().unwrap();
            let name = labels
                .first_child()
                .unwrap()
                .downcast::<gtk::Label>()
                .unwrap();
            let email = labels
                .last_child()
                .unwrap()
                .downcast::<gtk::Label>()
                .unwrap();
            assert_eq!(name.text(), "Jane Doe");
            assert_eq!(email.text(), "jane@example.com");
            assert!(
                name.color().red() < 0.2 && email.color().red() < 0.4,
                "both labels must stay dark on the white popup in dark mode"
            );
            let surface = popup
                .surface()
                .unwrap()
                .downcast::<gtk::gdk::Popup>()
                .unwrap();
            let input = entry.compute_bounds(&window).unwrap();
            let content = popup.child().unwrap().compute_bounds(&popup).unwrap();
            let popup_top = surface.position_y() as f32 + content.y();
            assert!(
                popup_top >= input.y() + input.height(),
                "popup content at {popup_top} overlaps input bottom {}",
                input.y() + input.height()
            );
            if index == 0 {
                if let Ok(path) = std::env::var("POSTBIRD_CONTACT_SCREENSHOT") {
                    let snapshot = gtk::Snapshot::new();
                    gtk::WidgetPaintable::new(Some(&popup)).snapshot(
                        &snapshot,
                        popup.width() as f64,
                        popup.height() as f64,
                    );
                    popup
                        .renderer()
                        .unwrap()
                        .render_texture(snapshot.to_node().unwrap(), None)
                        .save_to_png(path)
                        .unwrap();
                }
                assert_eq!(state.key(gtk::gdk::Key::Down), glib::Propagation::Stop);
                assert_eq!(list.selected_row().unwrap().index(), 1);
                state.key(gtk::gdk::Key::Up);
                state.key(gtk::gdk::Key::Return);
            } else {
                list.emit_by_name::<()>("row-activated", &[&row]);
            }
            assert_eq!(
                entry.text(),
                "\"Other, Person\" <other@example.com>, Jane Doe <jane@example.com>, last@example.com"
            );
            assert!(!popup.is_visible());
            entry.set_text("jane@");
            entry.set_position(-1);
            wait(&|| popup.is_visible());
            assert_eq!(state.matches.borrow().len(), 1);
            state.key(gtk::gdk::Key::Escape);
            assert_eq!(entry.text(), "jane@");
            assert!(!popup.is_visible());
        }
        dialog.close();
        window.destroy();
    }

    #[test]
    fn completion_respects_quoted_names_and_existing_recipients() {
        let text = "\"Doe, Jane\" <jane@example.com>, al, bob@example.com";
        let (start, end) = recipient_span(text, text.find("al,").unwrap() + 2);
        assert_eq!(&text[start..end], " al");
        assert_eq!(&text[..start], "\"Doe, Jane\" <jane@example.com>,");
        assert_eq!(&text[end..], ", bob@example.com");
        assert_eq!(recipient_span("Zoë", "Zoë".len()), (0, 4));
    }
}
