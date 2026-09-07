use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use adw::prelude::*;
use chrono::{Datelike, Local};
use gtk::{Align, Orientation, gio, glib};
use lettre::message::{Mailbox, Mailboxes};
use webkit6::prelude::*;

use crate::{
    accounts::{Account, AccountStore},
    cache::MailCache,
    gmail::{ComposeMessage, GmailClient, Message, Payload},
    preferences::UiPreferences,
};

#[derive(Clone)]
struct Widgets {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    accounts: gtk::StringList,
    account_picker: gtk::DropDown,
    messages: gtk::ListBox,
    labels: gtk::ListBox,
    mailbox_title: gtk::Label,
    message_title: gtk::Label,
    message_sender: gtk::Label,
    conversation_scroll: gtk::ScrolledWindow,
    conversation_body: gtk::Box,
    message_content: gtk::Box,
    detail_stack: gtk::Stack,
    archive: gtk::Button,
    star: gtk::Button,
    trash: gtk::Button,
    unread: gtk::Button,
    reply: gtk::Button,
    reply_all: gtk::Button,
    search: gtk::SearchEntry,
    preferences: Rc<RefCell<UiPreferences>>,
    load_images: gtk::Button,
}

struct State {
    account_emails: Vec<String>,
    labels: Vec<(String, String)>,
    conversations: Vec<Vec<Message>>,
    selected: Option<Message>,
    selected_conversation: Vec<Message>,
    current_label: String,
    load_generation: u64,
    inbox_sync_in_progress: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            account_emails: Vec::new(),
            labels: Vec::new(),
            conversations: Vec::new(),
            selected: None,
            selected_conversation: Vec::new(),
            current_label: "INBOX".to_owned(),
            load_generation: 0,
            inbox_sync_in_progress: false,
        }
    }
}

pub fn build(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Postbird")
        .default_width(1180)
        .default_height(760)
        .build();
    let toast = adw::ToastOverlay::new();
    let state = Rc::new(RefCell::new(State::default()));
    let preferences = Rc::new(RefCell::new(UiPreferences::load()));

    let toolbar_view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let compose = gtk::Button::builder()
        .label("Compose")
        .css_classes(["suggested-action"])
        .build();
    let add_account = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Add Google account")
        .build();
    let remove_account = gtk::Button::builder()
        .icon_name("edit-delete-symbolic")
        .tooltip_text("Remove current account")
        .build();
    header.pack_start(&compose);
    header.pack_end(&add_account);
    header.pack_end(&remove_account);
    toolbar_view.add_top_bar(&header);

    let accounts = gtk::StringList::new(&[]);
    let account_picker = gtk::DropDown::builder()
        .model(&accounts)
        .hexpand(true)
        .build();
    let messages = gtk::ListBox::new();
    messages.add_css_class("navigation-sidebar");
    messages.set_selection_mode(gtk::SelectionMode::Single);
    let labels = gtk::ListBox::new();
    labels.add_css_class("navigation-sidebar");
    labels.set_selection_mode(gtk::SelectionMode::Single);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search mail")
        .build();
    let mailbox_title = detail_label("Inbox", "title-2");
    let message_title = detail_label("Select a message", "title-1");
    let message_sender = detail_label("", "dim-label");
    let conversation_body = gtk::Box::new(Orientation::Vertical, 8);
    let conversation_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&conversation_body)
        .build();
    let message_content = gtk::Box::new(Orientation::Vertical, 8);
    message_content.set_visible(false);
    message_content.set_margin_top(18);
    message_content.set_margin_start(24);
    message_content.set_margin_end(24);
    message_content.append(&message_title);
    message_content.append(&message_sender);
    message_content.append(&conversation_scroll);

    let archive = action_button("mail-archive-symbolic", "Archive");
    let star = action_button("starred-symbolic", "Star");
    let trash = action_button("user-trash-symbolic", "Move to Trash");
    let unread = action_button("mail-mark-unread-symbolic", "Mark Unread");
    let reply = action_button("mail-reply-sender-symbolic", "Reply");
    let reply_all = action_button("mail-reply-all-symbolic", "Reply All");
    let load_images = action_button("image-x-generic-symbolic", "Always load remote images");
    let detail_actions = gtk::Box::new(Orientation::Horizontal, 6);
    detail_actions.append(&archive);
    detail_actions.append(&star);
    detail_actions.append(&trash);
    detail_actions.append(&unread);
    detail_actions.prepend(&reply);
    detail_actions.insert_child_after(&reply_all, Some(&reply));
    detail_actions.append(&load_images);
    message_content.prepend(&detail_actions);

    let message_status = adw::StatusPage::builder()
        .icon_name("mail-read-symbolic")
        .title("Select a message")
        .description("Choose a conversation to read it here.")
        .hexpand(true)
        .build();
    let detail_stack = gtk::Stack::new();
    detail_stack.add_named(&message_status, Some("status"));
    detail_stack.add_named(&message_content, Some("message"));
    detail_stack.set_visible_child_name("status");

    let widgets = Widgets {
        window: window.clone(),
        toast: toast.clone(),
        accounts,
        account_picker,
        messages,
        labels,
        mailbox_title,
        message_title,
        message_sender,
        conversation_scroll: conversation_scroll.clone(),
        conversation_body,
        message_content,
        detail_stack: detail_stack.clone(),
        archive,
        star,
        trash,
        unread,
        reply,
        reply_all,
        search,
        preferences: preferences.clone(),
        load_images,
    };
    if preferences.borrow().load_remote_images {
        widgets.load_images.set_sensitive(false);
        widgets
            .load_images
            .set_tooltip_text(Some("Remote images enabled"));
    }

    let message_split = gtk::Paned::new(Orientation::Horizontal);
    message_split.set_start_child(Some(&message_list_panel(&widgets)));
    message_split.set_end_child(Some(&detail_stack));
    message_split.set_position(preferences.borrow().message_split);
    message_split.set_wide_handle(true);
    message_split.set_resize_start_child(false);
    message_split.set_resize_end_child(true);
    message_split.set_shrink_start_child(false);
    message_split.set_shrink_end_child(false);
    message_split.set_hexpand(true);

    let mailbox_split = gtk::Paned::new(Orientation::Horizontal);
    mailbox_split.set_start_child(Some(&mailbox_sidebar(&widgets, &state)));
    mailbox_split.set_end_child(Some(&message_split));
    mailbox_split.set_position(preferences.borrow().mailbox_split);
    mailbox_split.set_wide_handle(true);
    mailbox_split.set_resize_start_child(false);
    mailbox_split.set_resize_end_child(true);
    mailbox_split.set_shrink_start_child(false);
    mailbox_split.set_shrink_end_child(false);
    toolbar_view.set_content(Some(&mailbox_split));
    toast.set_child(Some(&toolbar_view));
    window.set_content(Some(&toast));

    let conversation_for_resize = widgets.conversation_body.clone();
    conversation_scroll.add_tick_callback(move |scroll, _| {
        size_conversation_sections(scroll, &conversation_for_resize);
        glib::ControlFlow::Continue
    });

    connect_split_preferences(&mailbox_split, &message_split, &preferences);

    connect_selection(&widgets, &state, &detail_stack);
    connect_account_picker(&widgets, &state);
    connect_add_account(&widgets, &state, &add_account);
    connect_remove_account(&widgets, &state, &remove_account);
    connect_search(&widgets, &state);
    connect_message_actions(&widgets, &state);
    connect_reply(&widgets, &state);
    connect_remote_images(&widgets, &state);
    connect_compose(&widgets, &state, &compose);
    load_accounts(&widgets, &state);
    connect_background_sync(&widgets, &state);
    window.present();
}

fn connect_background_sync(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    glib::timeout_add_seconds_local(60, move || {
        sync_recent_inbox(&widgets, &state);
        glib::ControlFlow::Continue
    });
}

fn connect_split_preferences(
    mailbox_split: &gtk::Paned,
    message_split: &gtk::Paned,
    preferences: &Rc<RefCell<UiPreferences>>,
) {
    let preferences_for_mailbox = preferences.clone();
    mailbox_split.connect_position_notify(move |split| {
        preferences_for_mailbox.borrow_mut().mailbox_split = split.position();
        let _ = preferences_for_mailbox.borrow().save();
    });
    let preferences_for_message = preferences.clone();
    message_split.connect_position_notify(move |split| {
        preferences_for_message.borrow_mut().message_split = split.position();
        let _ = preferences_for_message.borrow().save();
    });
}

fn mailbox_sidebar(widgets: &Widgets, state: &Rc<RefCell<State>>) -> gtk::Widget {
    let sidebar = gtk::Box::new(Orientation::Vertical, 12);
    sidebar.set_size_request(160, -1);
    sidebar.set_margin_top(12);
    sidebar.set_margin_bottom(12);
    sidebar.set_margin_start(12);
    sidebar.set_margin_end(12);
    sidebar.append(&widgets.account_picker);
    let folders = gtk::ListBox::new();
    folders.add_css_class("navigation-sidebar");
    folders.set_selection_mode(gtk::SelectionMode::Single);
    for (icon, name) in [
        ("mail-unread-symbolic", "Inbox"),
        ("starred-symbolic", "Starred"),
        ("document-send-symbolic", "Sent"),
        ("document-edit-symbolic", "Drafts"),
        ("user-trash-symbolic", "Trash"),
    ] {
        let row = adw::ActionRow::builder()
            .title(name)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        folders.append(&row);
    }
    if let Some(row) = folders.row_at_index(0) {
        folders.select_row(Some(&row));
    }
    let labels_for_folder = widgets.labels.clone();
    let widgets_for_folder = widgets.clone();
    let state_for_folder = state.clone();
    folders.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        labels_for_folder.unselect_all();
        let (label, title) = match row.index() {
            1 => ("STARRED", "Starred"),
            2 => ("SENT", "Sent"),
            3 => ("DRAFT", "Drafts"),
            4 => ("TRASH", "Trash"),
            _ => ("INBOX", "Inbox"),
        };
        state_for_folder.borrow_mut().current_label = label.to_owned();
        widgets_for_folder.mailbox_title.set_text(title);
        let index = widgets_for_folder.account_picker.selected() as usize;
        let email = { state_for_folder.borrow().account_emails.get(index).cloned() };
        if let Some(email) = email {
            load_inbox(&widgets_for_folder, &state_for_folder, email, None, true);
        }
    });
    let folders_for_label = folders.clone();
    let widgets_for_label = widgets.clone();
    let state_for_label = state.clone();
    widgets.labels.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        folders_for_label.unselect_all();
        let selected = state_for_label
            .borrow()
            .labels
            .get(row.index() as usize)
            .cloned();
        let Some((label_id, label_name)) = selected else {
            return;
        };
        state_for_label.borrow_mut().current_label = label_id;
        widgets_for_label.mailbox_title.set_text(&label_name);
        let index = widgets_for_label.account_picker.selected() as usize;
        let email = state_for_label.borrow().account_emails.get(index).cloned();
        if let Some(email) = email {
            load_inbox(&widgets_for_label, &state_for_label, email, None, true);
        }
    });
    let navigation = gtk::Box::new(Orientation::Vertical, 10);
    navigation.append(&folders);
    navigation.append(&gtk::Separator::new(Orientation::Horizontal));
    let labels_heading = gtk::Label::builder()
        .label("Labels")
        .halign(Align::Start)
        .css_classes(["heading"])
        .build();
    navigation.append(&labels_heading);
    navigation.append(&widgets.labels);
    sidebar.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&navigation)
            .build(),
    );
    sidebar.upcast()
}

fn message_list_panel(widgets: &Widgets) -> gtk::Widget {
    let panel = gtk::Box::new(Orientation::Vertical, 8);
    panel.set_size_request(260, -1);
    panel.set_margin_top(12);
    panel.set_margin_bottom(12);
    panel.set_margin_start(12);
    panel.set_margin_end(12);
    panel.append(&widgets.mailbox_title);
    panel.append(&widgets.search);
    panel.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&widgets.messages)
            .build(),
    );
    panel.upcast()
}

fn connect_selection(widgets: &Widgets, state: &Rc<RefCell<State>>, stack: &gtk::Stack) {
    let widgets = widgets.clone();
    let state = state.clone();
    let stack = stack.clone();
    widgets
        .messages
        .clone()
        .connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let row_index = row.index() as usize;
            let Some(mut conversation) = state.borrow().conversations.get(row_index).cloned()
            else {
                return;
            };
            let unread_ids = conversation
                .iter()
                .filter(|message| message.label_ids.iter().any(|label| label == "UNREAD"))
                .map(|message| message.id.clone())
                .collect::<Vec<_>>();
            if !unread_ids.is_empty() {
                for message in &mut conversation {
                    message.label_ids.retain(|label| label != "UNREAD");
                }
                row.remove_css_class("accent");
                state.borrow_mut().conversations[row_index] = conversation.clone();
                mark_read_in_background(&widgets, &state, unread_ids);
            }
            let Some(message) = conversation.first().cloned() else {
                return;
            };
            widgets.message_title.set_text(message.header("Subject"));
            widgets
                .message_sender
                .set_text(&message_recipient_summary(&message, conversation.len()));
            display_conversation(
                &widgets,
                &state,
                &conversation,
                widgets.preferences.borrow().load_remote_images,
            );
            widgets.message_content.set_visible(true);
            stack.set_visible_child_name("message");
            state.borrow_mut().selected = Some(message);
            state.borrow_mut().selected_conversation = conversation;
        });
}

fn mark_read_in_background(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    message_ids: Vec<String>,
) {
    let index = widgets.account_picker.selected() as usize;
    let Some(email) = state.borrow().account_emails.get(index).cloned() else {
        return;
    };
    let widgets = widgets.clone();
    glib::MainContext::default().spawn_local(async move {
        let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
            let mut client = GmailClient::for_account(AccountStore::open()?, &email)?;
            for id in &message_ids {
                client.set_unread(id, false)?;
            }
            MailCache::open()?.mark_read(&email, &message_ids)
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => show_message(&widgets, &format!("Could not mark as read: {error}")),
            Err(_) => show_message(&widgets, "The mark-as-read task stopped unexpectedly"),
        }
    });
}

fn connect_account_picker(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    widgets
        .account_picker
        .clone()
        .connect_selected_notify(move |picker| {
            let index = picker.selected() as usize;
            let email = { state.borrow().account_emails.get(index).cloned() };
            if let Some(email) = email {
                load_labels(&widgets, &state, email.clone());
                load_inbox(&widgets, &state, email, None, false);
            }
        });
}

fn connect_add_account(widgets: &Widgets, state: &Rc<RefCell<State>>, button: &gtk::Button) {
    let widgets = widgets.clone();
    let state = state.clone();
    button.connect_clicked(move |_| {
        let widgets = widgets.clone();
        let state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            let store = match AccountStore::open() {
                Ok(store) => store,
                Err(error) => return show_error(&widgets, error),
            };
            if !store.credentials_path().exists() {
                let dialog = gtk::FileDialog::builder()
                    .title("Choose Google Desktop OAuth credentials")
                    .accept_label("Import")
                    .build();
                let file = match dialog.open_future(Some(&widgets.window)).await {
                    Ok(file) => file,
                    Err(error) if error.matches(gtk::DialogError::Dismissed) => return,
                    Err(error) => return show_error(&widgets, error),
                };
                let Some(path) = file.path() else {
                    return show_message(&widgets, "Please choose a local credentials JSON file");
                };
                if let Err(error) = store.import_credentials(&path) {
                    return show_error(&widgets, error);
                }
            }
            show_message(&widgets, "Finish signing in using your browser");
            let result = gio::spawn_blocking(move || -> anyhow::Result<Account> {
                let credentials = store.credentials()?;
                let token = credentials.authorize()?;
                let mut client =
                    GmailClient::new(credentials, token.clone(), String::new(), store.clone())?;
                let profile = client.profile()?;
                let account = Account {
                    display_name: profile.email_address.clone(),
                    email: profile.email_address,
                };
                store.save_account(account.clone(), &token)?;
                Ok(account)
            })
            .await;
            match result {
                Ok(Ok(account)) => {
                    load_accounts(&widgets, &state);
                    let index = {
                        state
                            .borrow()
                            .account_emails
                            .iter()
                            .position(|email| email == &account.email)
                    };
                    if let Some(index) = index {
                        widgets.account_picker.set_selected(index as u32);
                    }
                }
                Ok(Err(error)) => show_error(&widgets, error),
                Err(_) => show_message(&widgets, "The sign-in task stopped unexpectedly"),
            }
        });
    });
}

fn connect_search(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    widgets.search.clone().connect_activate(move |search| {
        let index = widgets.account_picker.selected() as usize;
        let Some(email) = state.borrow().account_emails.get(index).cloned() else {
            return;
        };
        let query = search.text().trim().to_owned();
        load_inbox(
            &widgets,
            &state,
            email,
            (!query.is_empty()).then_some(query),
            true,
        );
    });
}

fn connect_remove_account(widgets: &Widgets, state: &Rc<RefCell<State>>, button: &gtk::Button) {
    let widgets = widgets.clone();
    let state = state.clone();
    button.connect_clicked(move |_| {
        let index = widgets.account_picker.selected() as usize;
        let Some(email) = state.borrow().account_emails.get(index).cloned() else {
            return;
        };
        let widgets = widgets.clone();
        let state = state.clone();
        glib::MainContext::default().spawn_local(async move {
            let dialog = adw::AlertDialog::builder()
                .heading("Remove this account?")
                .body(format!(
                    "Remove {email} and its stored sign-in token from Postbird? This does not delete the Google account or any Gmail messages."
                ))
                .close_response("cancel")
                .default_response("cancel")
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove Account")]);
            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            if dialog.choose_future(Some(&widgets.window)).await != "remove" {
                return;
            }
            let result =
                gio::spawn_blocking(move || AccountStore::open()?.remove_account(&email)).await;
            match result {
                Ok(Ok(())) => load_accounts(&widgets, &state),
                Ok(Err(error)) => show_error(&widgets, error),
                Err(_) => show_message(&widgets, "The account removal task stopped unexpectedly"),
            }
        });
    });
}

fn connect_message_actions(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    for (button, action) in [
        (widgets.archive.clone(), "archive"),
        (widgets.star.clone(), "star"),
        (widgets.trash.clone(), "trash"),
        (widgets.unread.clone(), "unread"),
    ] {
        let widgets = widgets.clone();
        let state = state.clone();
        button.connect_clicked(move |button| {
            let Some(message) = state.borrow().selected.clone() else {
                return;
            };
            let conversation = state.borrow().selected_conversation.clone();
            let index = widgets.account_picker.selected() as usize;
            let Some(email) = state.borrow().account_emails.get(index).cloned() else {
                return;
            };
            if action == "archive" {
                set_archive_busy(button, true);
            }
            let action = action.to_owned();
            let action_for_request = action.clone();
            let archived_thread_id = message.thread_id.clone();
            let widgets_async = widgets.clone();
            let state_async = state.clone();
            glib::MainContext::default().spawn_local(async move {
                let email_for_reload = email.clone();
                let message_ids = conversation
                    .iter()
                    .map(|item| item.id.clone())
                    .collect::<Vec<_>>();
                let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                    let mut client = GmailClient::for_account(AccountStore::open()?, &email)?;
                    match action_for_request.as_str() {
                        "archive" => {
                            for item in &conversation {
                                client.archive(&item.id)?;
                            }
                            let _ = MailCache::open()
                                .and_then(|mut cache| cache.remove_messages(&email, &message_ids));
                            Ok(())
                        }
                        "star" => client.set_starred(
                            &message.id,
                            !message.label_ids.iter().any(|l| l == "STARRED"),
                        ),
                        "trash" => {
                            for item in &conversation {
                                client.trash(&item.id)?;
                            }
                            Ok(())
                        }
                        "unread" => {
                            for item in &conversation {
                                client.set_unread(&item.id, true)?;
                            }
                            Ok(())
                        }
                        _ => Ok(()),
                    }
                })
                .await;
                if action == "archive" {
                    set_archive_busy(&widgets_async.archive, false);
                }
                match result {
                    Ok(Ok(())) => {
                        let account_index = widgets_async.account_picker.selected() as usize;
                        let same_account = state_async.borrow().account_emails.get(account_index)
                            == Some(&email_for_reload);
                        if action == "archive"
                            && same_account
                            && state_async.borrow().current_label == "INBOX"
                        {
                            remove_conversation(&widgets_async, &state_async, &archived_thread_id);
                        } else {
                            load_inbox(&widgets_async, &state_async, email_for_reload, None, true);
                        }
                    }
                    Ok(Err(error)) => show_error(&widgets_async, error),
                    Err(_) => {
                        show_message(&widgets_async, "The message action stopped unexpectedly")
                    }
                }
            });
        });
    }
}

fn connect_compose(widgets: &Widgets, state: &Rc<RefCell<State>>, button: &gtk::Button) {
    let widgets = widgets.clone();
    let state = state.clone();
    button.connect_clicked(move |_| {
        let index = widgets.account_picker.selected() as usize;
        let Some(email) = state.borrow().account_emails.get(index).cloned() else {
            return show_message(&widgets, "Connect a Google account before composing");
        };
        present_compose(&widgets, email, None, None);
    });
}

fn connect_reply(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    connect_reply_button(widgets, state, &widgets.reply, false);
    connect_reply_button(widgets, state, &widgets.reply_all, true);
}

fn connect_reply_button(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    button: &gtk::Button,
    reply_all: bool,
) {
    let widgets = widgets.clone();
    let state = state.clone();
    button.clone().connect_clicked(move |_| {
        let index = widgets.account_picker.selected() as usize;
        let Some(email) = state.borrow().account_emails.get(index).cloned() else {
            return;
        };
        let Some(original) = state.borrow().selected.clone() else {
            return;
        };
        let subject = original.header("Subject");
        let (to, cc) = if reply_all {
            reply_all_recipients(&original, &email)
        } else {
            (reply_target(&original), String::new())
        };
        let reply = ComposeMessage {
            to,
            cc,
            bcc: String::new(),
            subject: if subject.to_lowercase().starts_with("re:") {
                subject.to_owned()
            } else {
                format!("Re: {subject}")
            },
            body: String::new(),
            html_body: None,
            in_reply_to: Some(original.header("Message-ID").to_owned()),
            thread_id: Some(original.thread_id.clone()),
            attachments: Vec::new(),
        };
        present_compose(&widgets, email, Some(reply), Some(original));
    });
}

fn connect_remote_images(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    widgets.load_images.clone().connect_clicked(move |button| {
        let mut preferences = widgets.preferences.borrow_mut();
        if !preferences.load_remote_images {
            preferences.load_remote_images = true;
            if let Err(error) = preferences.save() {
                preferences.load_remote_images = false;
                return show_error(&widgets, error);
            }
        }
        drop(preferences);
        button.set_sensitive(false);
        button.set_tooltip_text(Some("Remote images enabled"));
        let conversation = state.borrow().selected_conversation.clone();
        if !conversation.is_empty() {
            display_conversation(&widgets, &state, &conversation, true);
        }
    });
}

fn reply_target(message: &Message) -> String {
    let reply_to = message.header("Reply-To").trim();
    if reply_to.is_empty() {
        message.header("From").to_owned()
    } else {
        reply_to.to_owned()
    }
}

fn reply_all_recipients(message: &Message, account_email: &str) -> (String, String) {
    let own_address = account_email.to_ascii_lowercase();
    let mut seen = HashSet::new();
    seen.insert(own_address);
    let mut to = Vec::new();
    let mut cc = Vec::new();

    append_unique_mailboxes(&reply_target(message), &mut to, &mut seen);
    append_unique_mailboxes(message.header("To"), &mut to, &mut seen);
    append_unique_mailboxes(message.header("Cc"), &mut cc, &mut seen);

    (format_mailboxes(&to), format_mailboxes(&cc))
}

fn append_unique_mailboxes(
    header: &str,
    recipients: &mut Vec<Mailbox>,
    seen: &mut HashSet<String>,
) {
    let Ok(mailboxes) = header.parse::<Mailboxes>() else {
        return;
    };
    for mailbox in mailboxes {
        if seen.insert(mailbox.email.to_string().to_ascii_lowercase()) {
            recipients.push(mailbox);
        }
    }
}

fn format_mailboxes(mailboxes: &[Mailbox]) -> String {
    mailboxes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn present_compose(
    widgets: &Widgets,
    account_email: String,
    initial: Option<ComposeMessage>,
    replying_to: Option<Message>,
) {
    let parent = widgets.window.clone();
    let dialog = adw::Dialog::builder()
        .title(if replying_to.is_some() {
            "Reply"
        } else {
            "New message"
        })
        .content_width(900)
        .content_height(720)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let send = gtk::Button::builder()
        .label("Send")
        .css_classes(["suggested-action"])
        .build();
    let save_draft = gtk::Button::builder().label("Save Draft").build();
    let attach = gtk::Button::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Attach a file")
        .build();
    header.pack_end(&send);
    header.pack_end(&save_draft);
    header.pack_start(&attach);
    toolbar.add_top_bar(&header);
    let form = gtk::Box::new(Orientation::Vertical, 8);
    form.set_margin_top(12);
    form.set_margin_bottom(12);
    form.set_margin_start(12);
    form.set_margin_end(12);
    let to = entry("To");
    let cc = entry("Cc");
    let bcc = entry("Bcc");
    let subject = entry("Subject");
    let body = gtk::TextView::builder()
        .vexpand(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let formatting = rich_text_toolbar(&body);
    let attachment_paths = Rc::new(RefCell::new(Vec::new()));
    let attachment_label = gtk::Label::builder()
        .halign(Align::Start)
        .css_classes(["dim-label"])
        .build();
    let parent_for_attachment = parent.clone();
    let paths_for_attachment = attachment_paths.clone();
    let label_for_attachment = attachment_label.clone();
    attach.connect_clicked(move |_| {
        let parent = parent_for_attachment.clone();
        let paths = paths_for_attachment.clone();
        let label = label_for_attachment.clone();
        glib::MainContext::default().spawn_local(async move {
            let picker = gtk::FileDialog::builder().title("Attach a file").build();
            match picker.open_future(Some(&parent)).await {
                Ok(file) => {
                    if let Some(path) = file.path() {
                        paths.borrow_mut().push(path);
                        let names = paths
                            .borrow()
                            .iter()
                            .filter_map(|path| path.file_name())
                            .map(|name| name.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(", ");
                        label.set_text(&format!("Attached: {names}"));
                    }
                }
                Err(error) if error.matches(gtk::DialogError::Dismissed) => {}
                Err(_) => label.set_text("Could not attach that file"),
            }
        });
    });
    if let Some(initial) = &initial {
        to.set_text(&initial.to);
        cc.set_text(&initial.cc);
        bcc.set_text(&initial.bcc);
        subject.set_text(&initial.subject);
        body.buffer().set_text(&initial.body);
    }
    body.add_css_class("card");
    for widget in [&to, &cc, &bcc, &subject] {
        form.append(widget);
    }
    form.append(&attachment_label);
    form.append(&formatting);
    form.append(&body);
    if let Some(message) = replying_to.as_ref() {
        form.append(&reply_context(message));
    }
    toolbar.set_content(Some(&form));
    dialog.set_child(Some(&toolbar));

    let widgets_for_send = widgets.clone();
    let widgets_for_draft = widgets.clone();
    let dialog_for_send = dialog.clone();
    let account_for_send = account_email.clone();
    let account_for_draft = account_email;
    let reply_reference = initial
        .as_ref()
        .and_then(|message| message.in_reply_to.clone());
    let reply_thread = initial
        .as_ref()
        .and_then(|message| message.thread_id.clone());
    let to_send = to.clone();
    let cc_send = cc.clone();
    let bcc_send = bcc.clone();
    let subject_send = subject.clone();
    let body_send = body.clone();
    let attachments_send = attachment_paths.clone();
    send.connect_clicked(move |button| {
        button.set_sensitive(false);
        let (plain_body, html_body) = rich_text_content(&body_send);
        let message = ComposeMessage {
            to: to_send.text().to_string(),
            cc: cc_send.text().to_string(),
            bcc: bcc_send.text().to_string(),
            subject: subject_send.text().to_string(),
            body: plain_body,
            html_body: Some(html_body),
            in_reply_to: reply_reference.clone(),
            thread_id: reply_thread.clone(),
            attachments: attachments_send.borrow().clone(),
        };
        let widgets = widgets_for_send.clone();
        let dialog = dialog_for_send.clone();
        let account_email = account_for_send.clone();
        let button = button.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                GmailClient::for_account(AccountStore::open()?, &account_email)?.send(&message)?;
                Ok(())
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    dialog.close();
                    show_message(&widgets, "Message sent");
                }
                Ok(Err(error)) => {
                    button.set_sensitive(true);
                    show_error(&widgets, error);
                }
                Err(_) => {
                    button.set_sensitive(true);
                    show_message(&widgets, "The send task stopped unexpectedly");
                }
            }
        });
    });

    let dialog_for_draft = dialog.clone();
    let reply_reference = initial
        .as_ref()
        .and_then(|message| message.in_reply_to.clone());
    let reply_thread = initial
        .as_ref()
        .and_then(|message| message.thread_id.clone());
    let attachments_draft = attachment_paths;
    save_draft.connect_clicked(move |button| {
        button.set_sensitive(false);
        let (plain_body, html_body) = rich_text_content(&body);
        let message = ComposeMessage {
            to: to.text().to_string(),
            cc: cc.text().to_string(),
            bcc: bcc.text().to_string(),
            subject: subject.text().to_string(),
            body: plain_body,
            html_body: Some(html_body),
            in_reply_to: reply_reference.clone(),
            thread_id: reply_thread.clone(),
            attachments: attachments_draft.borrow().clone(),
        };
        let widgets = widgets_for_draft.clone();
        let dialog = dialog_for_draft.clone();
        let account_email = account_for_draft.clone();
        let button = button.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                GmailClient::for_account(AccountStore::open()?, &account_email)?
                    .create_draft(&message)?;
                Ok(())
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    dialog.close();
                    show_message(&widgets, "Draft saved");
                }
                Ok(Err(error)) => {
                    button.set_sensitive(true);
                    show_error(&widgets, error);
                }
                Err(_) => {
                    button.set_sensitive(true);
                    show_message(&widgets, "The draft task stopped unexpectedly");
                }
            }
        });
    });
    dialog.present(Some(&parent));
}

fn reply_context(message: &Message) -> gtk::Expander {
    let sender = sender_name(message.header("From"));
    let subject = message.header("Subject");
    let heading = if subject.trim().is_empty() {
        format!("Replying to {sender}")
    } else {
        format!("Replying to {sender} — {subject}")
    };
    let details = gtk::Label::builder()
        .label(format!(
            "From: {}\nDate: {}",
            message.header("From"),
            message.header("Date")
        ))
        .halign(Align::Start)
        .xalign(0.0)
        .selectable(true)
        .css_classes(["caption", "dim-label"])
        .build();
    let preview = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(10)
        .right_margin(10)
        .build();
    preview.buffer().set_text(&message.body_text());
    preview.add_css_class("card");
    let content = gtk::Box::new(Orientation::Vertical, 6);
    content.append(&details);
    content.append(
        &gtk::ScrolledWindow::builder()
            .height_request(160)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&preview)
            .build(),
    );
    gtk::Expander::builder()
        .label(heading)
        .expanded(true)
        .child(&content)
        .build()
}

const RICH_TAGS: [(&str, &str, &str); 4] = [
    ("postbird-bold", "<strong>", "</strong>"),
    ("postbird-italic", "<em>", "</em>"),
    ("postbird-underline", "<u>", "</u>"),
    ("postbird-strike", "<s>", "</s>"),
];

fn rich_text_toolbar(editor: &gtk::TextView) -> gtk::Box {
    let buffer = editor.buffer();
    for tag in [
        gtk::TextTag::builder()
            .name(RICH_TAGS[0].0)
            .weight(700)
            .build(),
        gtk::TextTag::builder()
            .name(RICH_TAGS[1].0)
            .style(gtk::pango::Style::Italic)
            .build(),
        gtk::TextTag::builder()
            .name(RICH_TAGS[2].0)
            .underline(gtk::pango::Underline::Single)
            .build(),
        gtk::TextTag::builder()
            .name(RICH_TAGS[3].0)
            .strikethrough(true)
            .build(),
    ] {
        buffer.tag_table().add(&tag);
    }
    let toolbar = gtk::Box::new(Orientation::Horizontal, 4);
    for (label, tooltip, tag_name) in [
        ("B", "Bold selected text", RICH_TAGS[0].0),
        ("I", "Italicize selected text", RICH_TAGS[1].0),
        ("U", "Underline selected text", RICH_TAGS[2].0),
        ("S", "Strikethrough selected text", RICH_TAGS[3].0),
    ] {
        let button = gtk::Button::builder()
            .label(label)
            .tooltip_text(tooltip)
            .css_classes(["flat"])
            .build();
        let buffer = buffer.clone();
        button.connect_clicked(move |_| toggle_selected_tag(&buffer, tag_name));
        toolbar.append(&button);
    }
    let clear = gtk::Button::builder()
        .label("Clear formatting")
        .tooltip_text("Remove formatting from selected text")
        .css_classes(["flat"])
        .build();
    clear.connect_clicked(move |_| {
        if let Some((start, end)) = buffer.selection_bounds() {
            buffer.remove_all_tags(&start, &end);
        }
    });
    toolbar.append(&clear);
    toolbar
}

fn toggle_selected_tag(buffer: &gtk::TextBuffer, tag_name: &str) {
    let Some((start, end)) = buffer.selection_bounds() else {
        return;
    };
    let Some(tag) = buffer.tag_table().lookup(tag_name) else {
        return;
    };
    if start.has_tag(&tag) {
        buffer.remove_tag(&tag, &start, &end);
    } else {
        buffer.apply_tag(&tag, &start, &end);
    }
}

fn rich_text_content(editor: &gtk::TextView) -> (String, String) {
    let buffer = editor.buffer();
    let plain = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    let tags = RICH_TAGS.map(|(name, _, _)| buffer.tag_table().lookup(name));
    let mut html = String::from(
        "<div style=\"font-family:system-ui,-apple-system,sans-serif;font-size:14px;line-height:1.5\">",
    );
    let mut active = [false; 4];
    let mut iter = buffer.start_iter();
    let end = buffer.end_iter();
    while iter.offset() < end.offset() {
        let next =
            std::array::from_fn(|index| tags[index].as_ref().is_some_and(|tag| iter.has_tag(tag)));
        if next != active {
            for index in (0..RICH_TAGS.len()).rev() {
                if active[index] {
                    html.push_str(RICH_TAGS[index].2);
                }
            }
            for index in 0..RICH_TAGS.len() {
                if next[index] {
                    html.push_str(RICH_TAGS[index].1);
                }
            }
            active = next;
        }
        match iter.char() {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '"' => html.push_str("&quot;"),
            '\n' => html.push_str("<br>\n"),
            character => html.push(character),
        }
        iter.forward_char();
    }
    for index in (0..RICH_TAGS.len()).rev() {
        if active[index] {
            html.push_str(RICH_TAGS[index].2);
        }
    }
    html.push_str("</div>");
    (plain, html)
}

fn load_accounts(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let store = match AccountStore::open() {
        Ok(store) => store,
        Err(error) => return show_error(widgets, error),
    };
    let accounts = match store.accounts() {
        Ok(accounts) => accounts,
        Err(error) => return show_error(widgets, error),
    };
    widgets.accounts.splice(0, widgets.accounts.n_items(), &[]);
    state.borrow_mut().account_emails.clear();
    for account in accounts {
        widgets.accounts.append(&account.display_name);
        state.borrow_mut().account_emails.push(account.email);
    }
    let has_accounts = !state.borrow().account_emails.is_empty();
    widgets.account_picker.set_sensitive(has_accounts);
    if has_accounts {
        widgets.account_picker.set_selected(0);
        let email = state.borrow().account_emails[0].clone();
        load_labels(widgets, state, email.clone());
        load_inbox(widgets, state, email, None, false);
    } else {
        clear_list(&widgets.labels);
        state.borrow_mut().labels.clear();
        clear_list(&widgets.messages);
        add_status_row(
            &widgets.messages,
            "No account connected",
            "Use the + button to add Google credentials and sign in.",
        );
    }
}

fn load_labels(widgets: &Widgets, state: &Rc<RefCell<State>>, email: String) {
    clear_list(&widgets.labels);
    add_status_row(&widgets.labels, "Loading labels…", "");
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let request_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<crate::gmail::Label>> {
            GmailClient::for_account(AccountStore::open()?, &request_email)?.labels()
        })
        .await;
        let index = widgets.account_picker.selected() as usize;
        if state.borrow().account_emails.get(index) != Some(&email) {
            return;
        }
        let mut labels = match result {
            Ok(Ok(labels)) => labels
                .into_iter()
                .filter(|label| label.kind.eq_ignore_ascii_case("user"))
                .collect::<Vec<_>>(),
            Ok(Err(error)) => {
                clear_list(&widgets.labels);
                add_label_row(&widgets.labels, "All Mail");
                state.borrow_mut().labels = vec![(String::new(), "All Mail".to_owned())];
                return show_message(&widgets, &format!("Could not load labels: {error}"));
            }
            Err(_) => {
                clear_list(&widgets.labels);
                add_label_row(&widgets.labels, "All Mail");
                state.borrow_mut().labels = vec![(String::new(), "All Mail".to_owned())];
                return show_message(&widgets, "The label task stopped unexpectedly");
            }
        };
        labels.sort_by_key(|label| label.name.to_lowercase());
        let mut available = vec![(String::new(), "All Mail".to_owned())];
        available.extend(labels.into_iter().map(|label| (label.id, label.name)));
        clear_list(&widgets.labels);
        for (_, name) in &available {
            add_label_row(&widgets.labels, name);
        }
        state.borrow_mut().labels = available;
    });
}

fn add_label_row(list: &gtk::ListBox, name: &str) {
    let row = adw::ActionRow::builder()
        .title(name)
        .use_markup(false)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("tag-symbolic"));
    list.append(&row);
}

fn sync_recent_inbox(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let email = {
        let mut state = state.borrow_mut();
        if state.inbox_sync_in_progress {
            return;
        }
        let index = widgets.account_picker.selected() as usize;
        let Some(email) = state.account_emails.get(index).cloned() else {
            return;
        };
        state.inbox_sync_in_progress = true;
        email
    };
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let sync_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<Message>> {
            let mut client = GmailClient::for_account(AccountStore::open()?, &sync_email)?;
            let page = client.list_threads_with_limit(Some("INBOX"), None, None, 50)?;
            let current_thread_ids = page
                .threads
                .iter()
                .map(|thread| thread.id.clone())
                .collect::<HashSet<_>>();
            let recent = page.threads.into_iter().take(10).collect::<Vec<_>>();
            let refreshed = fetch_threads_parallel(&sync_email, recent)?;
            let refreshed_thread_ids = refreshed
                .iter()
                .map(|message| message.thread_id.clone())
                .collect::<HashSet<_>>();
            let mut combined = MailCache::open()?.messages(&sync_email, "INBOX")?;
            combined.retain(|message| {
                current_thread_ids.contains(&message.thread_id)
                    && !refreshed_thread_ids.contains(&message.thread_id)
            });
            combined.extend(refreshed);
            combined.sort_by_key(|message| std::cmp::Reverse(message_timestamp(message)));
            MailCache::open()?.replace_mailbox(&sync_email, "INBOX", &combined)?;
            Ok(combined)
        })
        .await;
        state.borrow_mut().inbox_sync_in_progress = false;
        let index = widgets.account_picker.selected() as usize;
        let inbox_is_visible = state.borrow().current_label == "INBOX"
            && widgets.search.text().trim().is_empty()
            && state.borrow().account_emails.get(index) == Some(&email);
        if inbox_is_visible
            && let Ok(Ok(messages)) = result
            && mailbox_changed(&state, &messages)
        {
            display_messages(&widgets, &state, messages);
        }
    });
}

fn mailbox_changed(state: &Rc<RefCell<State>>, messages: &[Message]) -> bool {
    let mut visible = state
        .borrow()
        .conversations
        .iter()
        .flatten()
        .map(message_snapshot)
        .collect::<Vec<_>>();
    let mut refreshed = messages.iter().map(message_snapshot).collect::<Vec<_>>();
    visible.sort();
    refreshed.sort();
    visible != refreshed
}

fn message_snapshot(message: &Message) -> (String, String, String, Vec<String>, String) {
    (
        message.id.clone(),
        message.thread_id.clone(),
        message.internal_date.clone(),
        message.label_ids.clone(),
        format!("{}{:?}", message.snippet, message.attachments()),
    )
}

fn load_inbox(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    email: String,
    query: Option<String>,
    select_first: bool,
) {
    reset_reader(widgets, state);
    clear_list(&widgets.messages);
    add_status_row(
        &widgets.messages,
        "Loading…",
        "Fetching messages from Gmail",
    );
    let widgets = widgets.clone();
    let state = state.clone();
    let (label, generation) = {
        let mut state = state.borrow_mut();
        state.load_generation += 1;
        (state.current_label.clone(), state.load_generation)
    };
    let cached_mailbox =
        (query.is_none() && matches!(label.as_str(), "INBOX" | "SENT")).then(|| label.clone());
    glib::MainContext::default().spawn_local(async move {
        let mut showing_cached_messages = false;
        if let Some(mailbox) = cached_mailbox.clone() {
            let cache_email = email.clone();
            let cached =
                gio::spawn_blocking(move || MailCache::open()?.messages(&cache_email, &mailbox))
                    .await;
            if state.borrow().load_generation != generation {
                return;
            }
            if let Ok(Ok(messages)) = cached
                && !messages.is_empty()
            {
                display_messages(&widgets, &state, messages);
                if select_first {
                    select_first_conversation(&widgets);
                } else {
                    clear_reader_view(&widgets, &state);
                }
                showing_cached_messages = true;
            }
        }

        let online_email = email.clone();
        let cache_email = email;
        let mailbox_to_cache = cached_mailbox;
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<Message>> {
            let mut client = GmailClient::for_account(AccountStore::open()?, &online_email)?;
            let label = (!label.is_empty()).then_some(label.as_str());
            let page = client.list_threads(label, query.as_deref(), None)?;
            let messages = fetch_threads_parallel(&online_email, page.threads)?;
            if let Some(mailbox) = mailbox_to_cache {
                MailCache::open()?.replace_mailbox(&cache_email, &mailbox, &messages)?;
            }
            Ok(messages)
        })
        .await;
        if state.borrow().load_generation != generation {
            return;
        }
        match result {
            Ok(Ok(messages))
                if should_display_loaded_messages(
                    showing_cached_messages,
                    mailbox_changed(&state, &messages),
                ) =>
            {
                display_messages(&widgets, &state, messages);
                if select_first {
                    select_first_conversation(&widgets);
                } else {
                    clear_reader_view(&widgets, &state);
                }
            }
            Ok(Ok(_)) => {
                if select_first && widgets.messages.selected_row().is_none() {
                    select_first_conversation(&widgets);
                }
            }
            Ok(Err(error)) if showing_cached_messages => {
                show_message(
                    &widgets,
                    &format!("Showing cached mail; refresh failed: {error}"),
                );
            }
            Ok(Err(error)) => {
                clear_list(&widgets.messages);
                add_status_row(&widgets.messages, "Could not load mail", &error.to_string());
                show_error(&widgets, error);
            }
            Err(_) => show_message(&widgets, "The inbox task stopped unexpectedly"),
        }
    });
}

fn should_display_loaded_messages(showing_cached_messages: bool, mailbox_changed: bool) -> bool {
    !showing_cached_messages || mailbox_changed
}

fn reset_reader(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    state.borrow_mut().conversations.clear();
    clear_reader_view(widgets, state);
}

fn clear_reader_view(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    widgets.messages.unselect_all();
    {
        let mut state = state.borrow_mut();
        state.selected = None;
        state.selected_conversation.clear();
    }
    while let Some(child) = widgets.conversation_body.first_child() {
        widgets.conversation_body.remove(&child);
    }
    widgets.message_content.set_visible(false);
    widgets.detail_stack.set_visible_child_name("status");
}

fn select_first_conversation(widgets: &Widgets) {
    if let Some(row) = widgets.messages.row_at_index(0) {
        widgets.messages.select_row(Some(&row));
    }
}

fn remove_conversation(widgets: &Widgets, state: &Rc<RefCell<State>>, thread_id: &str) {
    let (row_index, was_selected) = {
        let state = state.borrow();
        let Some(row_index) = state.conversations.iter().position(|conversation| {
            conversation
                .first()
                .is_some_and(|message| message.thread_id == thread_id)
        }) else {
            return;
        };
        let was_selected = state
            .selected
            .as_ref()
            .is_some_and(|message| message.thread_id == thread_id);
        (row_index, was_selected)
    };

    if was_selected {
        clear_reader_view(widgets, state);
    }
    state.borrow_mut().conversations.remove(row_index);
    if let Some(row) = widgets.messages.row_at_index(row_index as i32) {
        widgets.messages.remove(&row);
    }

    if state.borrow().conversations.is_empty() {
        clear_reader_view(widgets, state);
        add_status_row(
            &widgets.messages,
            "Inbox is empty",
            "No messages matched this view.",
        );
        return;
    }
    if !was_selected {
        return;
    }
    let next_index = row_index.min(state.borrow().conversations.len() - 1) as i32;
    if let Some(row) = widgets.messages.row_at_index(next_index) {
        widgets.messages.select_row(Some(&row));
    } else {
        clear_reader_view(widgets, state);
    }
}

fn fetch_threads_parallel(
    email: &str,
    references: Vec<crate::gmail::ThreadRef>,
) -> anyhow::Result<Vec<Message>> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = references.len().min(8);
    let chunk_size = references.len().div_ceil(worker_count);
    let indexed = references.into_iter().enumerate().collect::<Vec<_>>();
    let batches = indexed
        .chunks(chunk_size)
        .map(|chunk| chunk.to_vec())
        .collect::<Vec<_>>();
    let mut messages = std::thread::scope(|scope| -> anyhow::Result<Vec<_>> {
        let handles = batches
            .into_iter()
            .map(|batch| {
                let email = email.to_owned();
                scope.spawn(move || -> anyhow::Result<Vec<_>> {
                    let mut client = GmailClient::for_account(AccountStore::open()?, &email)?;
                    batch
                        .into_iter()
                        .map(|(index, reference)| {
                            Ok((index, client.thread(&reference.id)?.messages))
                        })
                        .collect()
                })
            })
            .collect::<Vec<_>>();
        let mut all = Vec::new();
        for handle in handles {
            let batch = handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("mail worker stopped unexpectedly")))?;
            all.extend(batch);
        }
        Ok(all)
    })?;
    messages.sort_by_key(|(index, _)| *index);
    Ok(messages
        .into_iter()
        .flat_map(|(_, thread)| thread)
        .collect())
}

fn display_messages(widgets: &Widgets, state: &Rc<RefCell<State>>, messages: Vec<Message>) {
    clear_list(&widgets.messages);
    let conversations = group_conversations(messages);
    state.borrow_mut().conversations = conversations;
    if state.borrow().conversations.is_empty() {
        return add_status_row(
            &widgets.messages,
            "Inbox is empty",
            "No messages matched this view.",
        );
    }
    for conversation in &state.borrow().conversations {
        let Some(message) = conversation.first() else {
            continue;
        };
        let starred = conversation
            .iter()
            .any(|message| message.label_ids.iter().any(|label| label == "STARRED"));
        let row = conversation_list_row(message, conversation.len(), starred);
        if conversation
            .iter()
            .any(|message| message.label_ids.iter().any(|label| label == "UNREAD"))
        {
            row.add_css_class("accent");
        }
        widgets.messages.append(&row);
    }
}

fn conversation_list_row(message: &Message, count: usize, starred: bool) -> gtk::ListBoxRow {
    let sender = gtk::Label::builder()
        .label(sender_name(message.header("From")))
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let date = gtk::Label::builder()
        .label(relative_message_date(message))
        .halign(Align::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let heading = gtk::Box::new(Orientation::Horizontal, 8);
    heading.append(&sender);
    if starred {
        heading.append(&gtk::Image::from_icon_name("starred-symbolic"));
    }
    heading.append(&date);

    let subject = gtk::Label::builder()
        .label(if count > 1 {
            format!("{} ({count})", message.header("Subject"))
        } else {
            message.header("Subject").to_owned()
        })
        .halign(Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let preview = gtk::Label::builder()
        .label(message_preview(&message.snippet))
        .halign(Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let content = gtk::Box::new(Orientation::Vertical, 2);
    content.set_margin_top(7);
    content.set_margin_bottom(7);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.append(&heading);
    content.append(&subject);
    content.append(&preview);
    gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .child(&content)
        .build()
}

fn sender_name(from: &str) -> String {
    let name = from
        .split('<')
        .next()
        .unwrap_or(from)
        .trim()
        .trim_matches('"');
    if name.is_empty() {
        from.trim_matches(['<', '>']).to_owned()
    } else {
        name.to_owned()
    }
}

fn relative_message_date(message: &Message) -> String {
    let parsed = chrono::DateTime::parse_from_rfc2822(message.header("Date"))
        .ok()
        .map(|date| date.with_timezone(&Local))
        .or_else(|| {
            message
                .internal_date
                .parse::<i64>()
                .ok()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|date| date.with_timezone(&Local))
        });
    let Some(date) = parsed else {
        return String::new();
    };
    let today = Local::now().date_naive();
    let message_date = date.date_naive();
    if message_date == today {
        date.format("%-I:%M%P").to_string()
    } else if message_date == today - chrono::Duration::days(1) {
        "Yesterday".to_owned()
    } else if date.year() == today.year() {
        date.format("%-d %b").to_string()
    } else {
        date.format("%-d %b %Y").to_string()
    }
}

fn message_preview(snippet: &str) -> String {
    html2text::from_read(snippet.as_bytes(), 200)
        .unwrap_or_else(|_| snippet.to_owned())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn group_conversations(messages: Vec<Message>) -> Vec<Vec<Message>> {
    let mut by_thread: HashMap<String, Vec<Message>> = HashMap::new();
    for message in messages {
        by_thread
            .entry(message.thread_id.clone())
            .or_default()
            .push(message);
    }
    let mut conversations = by_thread.into_values().collect::<Vec<_>>();
    for conversation in &mut conversations {
        conversation.sort_by_key(|message| std::cmp::Reverse(message_timestamp(message)));
    }
    conversations.sort_by_key(|conversation| {
        std::cmp::Reverse(
            conversation
                .first()
                .map(message_timestamp)
                .unwrap_or_default(),
        )
    });
    conversations
}

fn message_timestamp(message: &Message) -> i64 {
    message.internal_date.parse().unwrap_or_default()
}

fn message_recipient_summary(message: &Message, count: usize) -> String {
    let mut lines = vec![format!(
        "Latest from: {}  •  {count} message{}",
        message.header("From"),
        if count == 1 { "" } else { "s" }
    )];
    append_address_line(&mut lines, "To", message.header("To"));
    append_address_line(&mut lines, "Cc", message.header("Cc"));
    append_address_line(&mut lines, "Bcc", message.header("Bcc"));
    lines.join("\n")
}

fn append_address_line(lines: &mut Vec<String>, label: &str, value: &str) {
    if !value.trim().is_empty() {
        lines.push(format!("{label}: {value}"));
    }
}

fn recipient_details(message: &Message) -> Option<gtk::Label> {
    let mut lines = Vec::new();
    append_address_line(&mut lines, "To", message.header("To"));
    append_address_line(&mut lines, "Cc", message.header("Cc"));
    append_address_line(&mut lines, "Bcc", message.header("Bcc"));
    if lines.is_empty() {
        return None;
    }
    Some(
        gtk::Label::builder()
            .label(lines.join("\n"))
            .halign(Align::Start)
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .margin_top(8)
            .margin_start(10)
            .margin_end(10)
            .css_classes(["caption", "dim-label"])
            .build(),
    )
}

fn attachment_filename(part: &Payload) -> String {
    let name = part
        .filename
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.trim().is_empty() && *name != "." && *name != "..")
        .unwrap_or("attachment");
    if std::path::Path::new(name).extension().is_none() {
        let mime_type = part.mime_type.split(';').next().unwrap_or("").trim();
        let extension = if mime_type.eq_ignore_ascii_case("message/rfc822") {
            Some("eml")
        } else if mime_type.eq_ignore_ascii_case("application/octet-stream") {
            None
        } else {
            mime_guess::get_mime_extensions_str(mime_type)
                .and_then(|extensions| extensions.first().copied())
        };
        if let Some(extension) = extension {
            return format!("{name}.{extension}");
        }
    }
    name.to_owned()
}

fn attachment_summary(attachments: &[&Payload]) -> String {
    match attachments {
        [] => String::new(),
        [part] => attachment_filename(part).to_owned(),
        [first, rest @ ..] => format!("{} (+{})", attachment_filename(first), rest.len()),
    }
}

fn attachment_control(widgets: &Widgets, email: &str, message: &Message) -> Option<gtk::Widget> {
    let attachments = message.attachments();
    if attachments.is_empty() {
        return None;
    }
    let popover = gtk::Popover::new();
    let list = gtk::Box::new(Orientation::Vertical, 4);
    for part in &attachments {
        let button = gtk::Button::new();
        let row = gtk::Box::new(Orientation::Horizontal, 6);
        row.append(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
        let label = gtk::Label::builder()
            .label(attachment_filename(part))
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(48)
            .xalign(0.0)
            .build();
        row.append(&label);
        button.set_child(Some(&row));
        button.set_tooltip_text(Some(&format!("Save {}", attachment_filename(part))));
        let widgets = widgets.clone();
        let email = email.to_owned();
        let message_id = message.id.clone();
        let part = (*part).clone();
        let popover = popover.downgrade();
        button.connect_clicked(move |button| {
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
            save_attachment(&widgets, &email, &message_id, &part, button);
        });
        if attachments.len() == 1 {
            button.set_valign(Align::Center);
            return Some(button.upcast());
        }
        list.append(&button);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(320)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    popover.set_child(Some(&scroll));
    let summary = attachment_summary(&attachments);
    let label = gtk::Label::builder()
        .label(&summary)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(36)
        .xalign(0.0)
        .build();
    let tooltip = attachments
        .iter()
        .map(|part| attachment_filename(part))
        .collect::<Vec<_>>()
        .join("\n");
    Some(
        gtk::MenuButton::builder()
            .child(&label)
            .always_show_arrow(true)
            .tooltip_text(format!("Save an attachment\n{tooltip}"))
            .popover(&popover)
            .valign(Align::Center)
            .build()
            .upcast(),
    )
}

fn save_attachment(
    widgets: &Widgets,
    email: &str,
    message_id: &str,
    part: &Payload,
    button: &gtk::Button,
) {
    let widgets = widgets.clone();
    let email = email.to_owned();
    let message_id = message_id.to_owned();
    let part = part.clone();
    let button = button.clone();
    button.set_sensitive(false);
    glib::MainContext::default().spawn_local(async move {
        let picker = gtk::FileDialog::builder()
            .title("Save attachment")
            .initial_name(attachment_filename(&part))
            .build();
        let file = match picker.save_future(Some(&widgets.window)).await {
            Ok(file) => file,
            Err(error) => {
                button.set_sensitive(true);
                if !error.matches(gtk::DialogError::Dismissed) {
                    show_message(&widgets, &format!("Could not save attachment: {error}"));
                }
                return;
            }
        };
        show_message(&widgets, "Downloading attachment…");
        let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
            let bytes = if let Some(data) = &part.body.data {
                crate::gmail::decode_attachment_data(data)?
            } else {
                GmailClient::for_account(AccountStore::open()?, &email)?
                    .attachment(&message_id, &part.body)?
            };
            file.replace_contents(
                &bytes,
                None,
                false,
                gio::FileCreateFlags::PRIVATE,
                gio::Cancellable::NONE,
            )?;
            Ok(())
        })
        .await;
        button.set_sensitive(true);
        match result {
            Ok(Ok(())) => show_message(&widgets, "Attachment saved"),
            Ok(Err(error)) => {
                show_message(&widgets, &format!("Could not save attachment: {error}"))
            }
            Err(_) => show_message(&widgets, "The attachment download stopped unexpectedly"),
        }
    });
}

fn display_conversation(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    conversation: &[Message],
    load_remote_images: bool,
) {
    let scroll = &widgets.conversation_scroll;
    let container = &widgets.conversation_body;
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    for (index, message) in conversation.iter().enumerate() {
        let heading = gtk::Label::builder()
            .label(format!(
                "{}\n{}",
                message.header("From"),
                message.header("Date")
            ))
            .halign(Align::Start)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption"])
            .build();
        let heading_row = gtk::Box::new(Orientation::Horizontal, 12);
        heading.set_hexpand(true);
        heading_row.append(&heading);
        if let Some(email) = state
            .borrow()
            .account_emails
            .get(widgets.account_picker.selected() as usize)
            && let Some(control) = attachment_control(widgets, email, message)
        {
            heading_row.append(&control);
        }
        let body = new_message_webview(load_remote_images);
        body.load_html(&message.rendered_body(), None);
        let expanded_content = gtk::Box::new(Orientation::Vertical, 6);
        if let Some(recipients) = recipient_details(message) {
            expanded_content.append(&recipients);
        }
        expanded_content.append(&body);
        let expander = gtk::Expander::builder()
            .label_widget(&heading_row)
            .child(&expanded_content)
            .expanded(index == 0)
            .build();
        let scroll_for_expander = scroll.clone();
        let container_for_expander = container.clone();
        expander.connect_expanded_notify(move |_| {
            size_conversation_sections(&scroll_for_expander, &container_for_expander);
        });
        expander.add_css_class("card");
        container.append(&expander);
    }
    size_conversation_sections(scroll, container);
}

fn size_conversation_sections(scroll: &gtk::ScrolledWindow, container: &gtk::Box) {
    let mut expanders = Vec::new();
    let mut child = container.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(expander) = widget.downcast::<gtk::Expander>() {
            expanders.push(expander);
        }
    }
    let expanded = expanders
        .iter()
        .filter(|expander| expander.is_expanded())
        .count() as i32;
    if expanded == 0 || scroll.height() <= 0 {
        return;
    }
    let headers_and_spacing = expanders.len() as i32 * 46;
    let height = ((scroll.height() - headers_and_spacing) / expanded).max(320);
    for expander in expanders {
        if expander.is_expanded()
            && let Some(view) = expander
                .child()
                .and_then(|child| child.downcast::<gtk::Box>().ok())
                .and_then(|content| content.last_child())
                .and_then(|child| child.downcast::<webkit6::WebView>().ok())
        {
            let body_height = (height - 64).max(256);
            if view.height_request() != body_height {
                view.set_height_request(body_height);
            }
        }
    }
}

fn new_message_webview(load_remote_images: bool) -> webkit6::WebView {
    let settings = webkit6::Settings::new();
    settings.set_enable_javascript(false);
    settings.set_enable_javascript_markup(false);
    settings.set_enable_html5_database(false);
    settings.set_enable_html5_local_storage(false);
    settings.set_auto_load_images(load_remote_images);
    let view = webkit6::WebView::builder()
        .settings(&settings)
        .network_session(&webkit6::NetworkSession::new_ephemeral())
        .hexpand(true)
        .build();
    view.connect_decide_policy(|_, decision, decision_type| {
        if !matches!(
            decision_type,
            webkit6::PolicyDecisionType::NavigationAction
                | webkit6::PolicyDecisionType::NewWindowAction
        ) {
            return false;
        }
        let Some(navigation) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else {
            return false;
        };
        let Some(mut action) = navigation.navigation_action() else {
            return false;
        };
        if action.navigation_type() != webkit6::NavigationType::LinkClicked {
            return false;
        }
        if let Some(uri) = action.request().and_then(|request| request.uri()) {
            gio::AppInfo::launch_default_for_uri_async(
                uri.as_str(),
                None::<&gio::AppLaunchContext>,
                None::<&gio::Cancellable>,
                move |result| {
                    if let Err(error) = result {
                        eprintln!("could not open link in the default application: {error}");
                    }
                },
            );
        }
        decision.ignore();
        true
    });
    view
}

fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}
fn add_status_row(list: &gtk::ListBox, title: &str, subtitle: &str) {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    list.append(&row);
}
fn show_error(widgets: &Widgets, error: impl std::fmt::Display) {
    let dialog = adw::AlertDialog::builder()
        .heading("Postbird could not complete that action")
        .body(error.to_string())
        .build();
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.present(Some(&widgets.window));
}
fn show_message(widgets: &Widgets, message: &str) {
    widgets.toast.add_toast(adw::Toast::new(message));
}
fn detail_label(text: &str, css_class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(Align::Start)
        .wrap(true)
        .selectable(true)
        .css_classes([css_class])
        .build()
}
fn action_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .tooltip_text(tooltip)
        .width_request(42)
        .height_request(42)
        .build();
    set_action_icon(&button, icon);
    button
}
fn set_action_icon(button: &gtk::Button, icon: &str) {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(20);
    button.set_child(Some(&image));
}
fn set_archive_busy(button: &gtk::Button, busy: bool) {
    button.set_sensitive(!busy);
    if busy {
        let spinner = gtk::Spinner::new();
        spinner.start();
        button.set_child(Some(&spinner));
        button.set_tooltip_text(Some("Archiving…"));
    } else {
        set_action_icon(button, "mail-archive-symbolic");
        button.set_tooltip_text(Some("Archive"));
    }
}
fn entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder().placeholder_text(placeholder).build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attachment_extensions_use_the_file_type_only_when_missing() {
        for (filename, mime_type, expected) in [
            ("image", "image/png", "image.png"),
            (
                "Forwarded message",
                "message/rfc822",
                "Forwarded message.eml",
            ),
            ("photo.jpeg", "image/jpeg", "photo.jpeg"),
            ("archive.tar.gz", "application/gzip", "archive.tar.gz"),
            ("data", "application/octet-stream", "data"),
            ("", "application/pdf", "attachment.pdf"),
        ] {
            let part = Payload {
                filename: filename.to_owned(),
                mime_type: mime_type.to_owned(),
                ..Payload::default()
            };
            assert_eq!(attachment_filename(&part), expected);
        }
    }

    #[test]
    fn attachment_summary_preserves_the_actual_filename_and_extension() {
        let first = Payload {
            filename: "Quarterly report – September 2026.final.pdf".to_owned(),
            ..Payload::default()
        };
        let second = Payload {
            filename: "source.backup.tar.gz".to_owned(),
            ..Payload::default()
        };
        assert_eq!(attachment_summary(&[&first]), first.filename);
        assert_eq!(
            attachment_summary(&[&first, &second]),
            "Quarterly report – September 2026.final.pdf (+1)"
        );
        assert_eq!(attachment_filename(&second), "source.backup.tar.gz");
    }

    #[test]
    fn attachment_names_are_safe_save_dialog_defaults() {
        for (filename, expected) in [
            ("../../report.pdf", "report.pdf"),
            ("C:\\files\\notes.txt", "notes.txt"),
            ("..", "attachment"),
            ("", "attachment"),
        ] {
            let part = Payload {
                filename: filename.to_owned(),
                ..Payload::default()
            };
            assert_eq!(attachment_filename(&part), expected);
        }
    }

    #[test]
    fn refreshed_attachment_metadata_changes_the_mailbox_snapshot() {
        let mut cached = message("one", "thread", 10);
        let before = message_snapshot(&cached);
        cached.payload.filename = "report.pdf".to_owned();
        assert_ne!(before, message_snapshot(&cached));
    }

    #[test]
    fn completed_initial_load_replaces_loading_row_even_when_empty() {
        assert!(should_display_loaded_messages(false, false));
        assert!(!should_display_loaded_messages(true, false));
        assert!(should_display_loaded_messages(true, true));
    }

    fn message(id: &str, thread: &str, timestamp: i64) -> Message {
        serde_json::from_value(json!({
            "id": id,
            "threadId": thread,
            "internalDate": timestamp.to_string(),
            "payload": {}
        }))
        .unwrap()
    }

    #[test]
    fn groups_threads_with_newest_conversation_and_message_first() {
        let grouped = group_conversations(vec![
            message("older", "one", 10),
            message("newest", "one", 30),
            message("middle", "two", 20),
        ]);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0][0].id, "newest");
        assert_eq!(grouped[0][1].id, "older");
        assert_eq!(grouped[1][0].id, "middle");
    }

    #[test]
    fn formats_sender_preview_and_relative_dates() {
        assert_eq!(
            sender_name("\"Jane Example\" <jane@example.com>"),
            "Jane Example"
        );
        assert_eq!(
            message_preview("Hello &amp; welcome\nagain"),
            "Hello & welcome again"
        );

        let today = message("today", "one", Local::now().timestamp_millis());
        assert!(
            relative_message_date(&today).ends_with("am")
                || relative_message_date(&today).ends_with("pm")
        );
        let yesterday = message(
            "yesterday",
            "two",
            (Local::now() - chrono::Duration::days(1)).timestamp_millis(),
        );
        assert_eq!(relative_message_date(&yesterday), "Yesterday");
    }

    #[test]
    fn background_snapshot_ignores_order_but_detects_mail_changes() {
        let first = message("one", "thread-one", 10);
        let second = message("two", "thread-two", 20);
        let state = Rc::new(RefCell::new(State::default()));
        state.borrow_mut().conversations = vec![vec![second.clone()], vec![first.clone()]];
        assert!(!mailbox_changed(&state, &[first.clone(), second.clone()]));

        let mut changed = second;
        changed.label_ids.push("STARRED".to_owned());
        assert!(mailbox_changed(&state, &[first, changed]));
    }

    #[test]
    fn reply_all_preserves_to_and_cc_and_excludes_the_account() {
        let original: Message = serde_json::from_value(json!({
            "id": "message",
            "threadId": "thread",
            "payload": { "headers": [
                { "name": "From", "value": "Alice <alice@example.com>" },
                { "name": "To", "value": "Kent <kent@example.com>, Bob <bob@example.com>" },
                { "name": "Cc", "value": "Carol <carol@example.com>, Bob <bob@example.com>" }
            ]}
        }))
        .unwrap();

        let (to, cc) = reply_all_recipients(&original, "kent@example.com");
        assert_eq!(to, "Alice <alice@example.com>, Bob <bob@example.com>");
        assert_eq!(cc, "Carol <carol@example.com>");
    }
}
