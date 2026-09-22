use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use adw::prelude::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Datelike, Local};
use gtk::{Align, Orientation, gio, glib};
use lettre::message::{Mailbox, Mailboxes};
use webkit6::prelude::*;

use crate::{
    accounts::{Account, AccountKind, AccountStore},
    cache::MailCache,
    gmail::{ComposeMessage, ForwardedAttachment, Message, Payload, ThreadRef},
    mail::{ImapClient, MailClient},
    preferences::UiPreferences,
};

#[derive(Clone)]
struct Widgets {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    accounts: gtk::StringList,
    account_picker: gtk::DropDown,
    messages: gtk::ListBox,
    sidebar_navigation: gtk::Box,
    mailbox_title: gtk::Label,
    mailbox_spinner: gtk::Spinner,
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
    forward: gtk::Button,
    search: gtk::SearchEntry,
    preferences: Rc<RefCell<UiPreferences>>,
    load_images: gtk::Button,
}

struct State {
    account_emails: Vec<String>,
    account_names: Vec<String>,
    account_labels: HashMap<String, Vec<(String, String)>>,
    unread_counts: HashMap<(String, String), u32>,
    unread_refresh_generation: HashMap<String, u64>,
    collapsed_accounts: HashSet<String>,
    conversations: Vec<Vec<Message>>,
    selected: Option<Message>,
    selected_conversation: Vec<Message>,
    history_expanded: bool,
    current_label: String,
    load_generation: u64,
    load_cancel: Arc<AtomicBool>,
    mailbox_loading: bool,
    inbox_sync_account: Option<String>,
    inbox_sync_cancel: Option<Arc<AtomicBool>>,
    pending_archives: HashSet<(String, String)>,
    archived_messages: HashMap<(String, String), (HashSet<String>, std::time::Instant)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            account_emails: Vec::new(),
            account_names: Vec::new(),
            account_labels: HashMap::new(),
            unread_counts: HashMap::new(),
            unread_refresh_generation: HashMap::new(),
            collapsed_accounts: HashSet::new(),
            conversations: Vec::new(),
            selected: None,
            selected_conversation: Vec::new(),
            history_expanded: false,
            current_label: "INBOX".to_owned(),
            load_generation: 0,
            load_cancel: Arc::new(AtomicBool::new(false)),
            mailbox_loading: false,
            inbox_sync_account: None,
            inbox_sync_cancel: None,
            pending_archives: HashSet::new(),
            archived_messages: HashMap::new(),
        }
    }
}

pub fn build(app: &adw::Application) {
    install_visual_style();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Postbird")
        .default_width(1180)
        .default_height(760)
        .build();
    window.add_css_class("postbird-window");
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
        .tooltip_text("Add mail account")
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
    let sidebar_navigation = gtk::Box::new(Orientation::Vertical, 5);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search mail")
        .build();
    let mailbox_title = detail_label("Inbox", "title-2");
    let mailbox_spinner = gtk::Spinner::builder()
        .width_request(18)
        .height_request(18)
        .valign(Align::Center)
        .visible(false)
        .tooltip_text("Syncing this folder…")
        .build();
    let message_title = detail_label("Select a message", "postbird-message-title");
    let message_sender = detail_label("", "dim-label");
    let conversation_body = gtk::Box::new(Orientation::Vertical, 4);
    let conversation_scroll = new_conversation_scroll(&conversation_body);
    let message_content = gtk::Box::new(Orientation::Vertical, 4);
    message_content.set_visible(false);
    message_content.set_margin_top(9);
    message_content.set_margin_start(12);
    message_content.set_margin_end(12);
    message_content.append(&message_title);
    message_content.append(&message_sender);
    message_content.append(&conversation_scroll);

    let archive = action_button("mail-archive-symbolic", "Archive");
    let star = action_button("starred-symbolic", "Star");
    let trash = action_button("user-trash-symbolic", "Move to Trash");
    let unread = action_button("mail-mark-unread-symbolic", "Mark Unread");
    let reply = action_button("mail-reply-sender-symbolic", "Reply");
    let forward = action_button("mail-forward-symbolic", "Forward");
    let reply_all = action_button("mail-reply-all-symbolic", "Reply All");
    let load_images = action_button("image-x-generic-symbolic", "Always load remote images");
    let detail_actions = gtk::Box::new(Orientation::Horizontal, 6);
    detail_actions.add_css_class("postbird-actions");
    let organise_actions = gtk::Box::new(Orientation::Horizontal, 0);
    organise_actions.add_css_class("postbird-action-group");
    for button in [&archive, &trash, &unread, &star] {
        button.add_css_class("flat");
        organise_actions.append(button);
    }
    let reply_actions = gtk::Box::new(Orientation::Horizontal, 0);
    reply_actions.add_css_class("postbird-action-group");
    for button in [&reply, &reply_all, &forward] {
        button.add_css_class("flat");
        reply_actions.append(button);
    }
    load_images.add_css_class("flat");
    detail_actions.append(&organise_actions);
    detail_actions.append(&reply_actions);
    detail_actions.append(&load_images);
    message_content.prepend(&detail_actions);

    let message_status = adw::StatusPage::builder()
        .icon_name("mail-read-symbolic")
        .title("Select a message")
        .description("Choose a conversation to read it here.")
        .hexpand(true)
        .build();
    let detail_stack = gtk::Stack::new();
    detail_stack.add_css_class("postbird-reader");
    detail_stack.add_named(&message_status, Some("status"));
    detail_stack.add_named(&message_content, Some("message"));
    detail_stack.set_visible_child_name("status");

    let widgets = Widgets {
        window: window.clone(),
        toast: toast.clone(),
        accounts,
        account_picker,
        messages,
        sidebar_navigation,
        mailbox_title,
        mailbox_spinner,
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
        forward,
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
    mailbox_split.set_position(preferences.borrow().mailbox_split.max(245));
    mailbox_split.set_wide_handle(true);
    mailbox_split.set_resize_start_child(false);
    mailbox_split.set_resize_end_child(true);
    mailbox_split.set_shrink_start_child(false);
    mailbox_split.set_shrink_end_child(false);
    toolbar_view.set_content(Some(&mailbox_split));
    toast.set_child(Some(&toolbar_view));
    window.set_content(Some(&toast));

    connect_split_preferences(&mailbox_split, &message_split, &preferences);

    connect_selection(&widgets, &state, &detail_stack);
    connect_account_picker(&widgets, &state);
    connect_add_account(&widgets, &state, &add_account);
    connect_remove_account(&widgets, &state, &remove_account);
    connect_search(&widgets, &state);
    connect_message_actions(&widgets, &state);
    connect_reply(&widgets, &state);
    connect_forward(&widgets, &state);
    connect_remote_images(&widgets, &state);
    connect_compose(&widgets, &state, &compose);
    load_accounts(&widgets, &state);
    connect_background_sync(&widgets, &state);
    window.present();
}

fn new_conversation_scroll(body: &gtk::Box) -> gtk::ScrolledWindow {
    // A WebView takes focus on the first text selection. GTK's default
    // viewport then scrolls to the entire (potentially very tall) WebView,
    // moving the conversation underneath the drag gesture.
    let viewport = gtk::Viewport::builder()
        .scroll_to_focus(false)
        .child(body)
        .build();
    gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&viewport)
        .build()
}

fn install_visual_style() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "window.postbird-window { font-size: 0.92em; }
         .postbird-sidebar { background: #202633; color: #eff3fb; }
         .postbird-sidebar label, .postbird-sidebar image { color: #eff3fb; }
         .postbird-sidebar .dim-label, .postbird-sidebar-heading { color: #aeb8c9; }
         .postbird-sidebar-heading { margin: 4px 4px 1px; letter-spacing: 0.08em; }
         .postbird-sidebar separator { background: #485062; margin: 5px 2px; }
         .postbird-sidebar expander { color: #eff3fb; }
         .postbird-account { margin-top: 2px; }
         .postbird-account-folders { margin: 2px 0 4px 8px; }
         .postbird-folder { min-height: 28px; padding: 2px 4px; border-radius: 7px; }
         .postbird-folder:hover { background: #39445a; }
         .postbird-folder.active { background: #315c9e; }
         .postbird-favorite { min-width: 24px; padding: 0; opacity: 0.72; }
         .postbird-sidebar .postbird-unread-badge { background: #7daaff; color: #15243d;
             border-radius: 99px; padding: 1px 6px; font-weight: bold; font-size: 0.85em; }
         .postbird-list-panel { background: #25282e; color: #edf0f5; }
         .postbird-list-panel label, .postbird-list-panel image { color: #edf0f5; }
         .postbird-list-panel .dim-label { color: #afb4bd; }
         .postbird-list-panel listbox, .postbird-list-panel row { background: transparent; }
         .postbird-list-panel row:hover { background: #343b49; }
         .postbird-list-panel row:selected { background: #185bb4; }
         .postbird-reader, .postbird-reader scrolledwindow { background: #ffffff; color: #181b20; }
         .postbird-reader label, .postbird-reader image { color: #181b20; }
         .postbird-reader .dim-label { color: #717984; }
         .postbird-reader .postbird-message-title { font-size: 18px; font-weight: 700; }
         .postbird-reader .postbird-part-trigger { min-width: 24px; min-height: 24px; padding: 2px; }
         .postbird-reader .message-section { background: #ffffff; color: #181b20;
             border: 0; border-bottom: 1px solid #e3e7ed; border-radius: 0; box-shadow: none; }
         .postbird-reader .postbird-history-toggle { background: #f0f4fa; border-radius: 12px;
             margin: 3px 0; min-height: 32px; }
         .postbird-reader .postbird-history-toggle label { color: #315c9e; }
         .postbird-action-group { background: #f2f4f7; border-radius: 12px; padding: 2px; }
         .postbird-action-group button { min-width: 34px; min-height: 30px; }
         .postbird-actions { margin-bottom: 11px; }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn connect_background_sync(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    glib::timeout_add_seconds_local(60, move || {
        sync_recent_inbox(&widgets, &state);
        refresh_favorite_counts(&widgets, &state);
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
    let sidebar = gtk::Box::new(Orientation::Vertical, 0);
    sidebar.add_css_class("postbird-sidebar");
    sidebar.set_size_request(225, -1);
    widgets.sidebar_navigation.set_margin_top(9);
    widgets.sidebar_navigation.set_margin_bottom(9);
    widgets.sidebar_navigation.set_margin_start(6);
    widgets.sidebar_navigation.set_margin_end(6);
    sidebar.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&widgets.sidebar_navigation)
            .build(),
    );
    rebuild_sidebar(widgets, state);
    sidebar.upcast()
}

fn sidebar_folders(state: &State, email: &str) -> Vec<(String, String, &'static str)> {
    let mut folders = [
        ("INBOX", "Inbox", "mail-unread-symbolic"),
        ("STARRED", "Starred", "starred-symbolic"),
        ("SENT", "Sent", "document-send-symbolic"),
        ("DRAFT", "Drafts", "document-edit-symbolic"),
        ("", "All Mail", "mail-archive-symbolic"),
        ("TRASH", "Trash", "user-trash-symbolic"),
    ]
    .into_iter()
    .map(|(id, title, icon)| (id.to_owned(), title.to_owned(), icon))
    .collect::<Vec<_>>();
    if let Some(labels) = state.account_labels.get(email) {
        folders.extend(
            labels
                .iter()
                .map(|(id, title)| (id.clone(), title.clone(), "tag-symbolic")),
        );
    }
    folders
}

fn sidebar_heading(title: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(title)
        .halign(Align::Start)
        .css_classes(["caption", "heading", "postbird-sidebar-heading"])
        .build()
}

fn sidebar_folder_row(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    email: &str,
    label_id: &str,
    title: &str,
    icon: &str,
    account_name: Option<&str>,
) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 1);
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("postbird-folder");
    let selected = state
        .borrow()
        .account_emails
        .get(widgets.account_picker.selected() as usize)
        .is_some_and(|active_email| active_email == email)
        && state.borrow().current_label == label_id;
    if selected {
        button.add_css_class("active");
    }
    button.set_hexpand(true);
    let content = gtk::Box::new(Orientation::Horizontal, 5);
    content.append(&gtk::Image::from_icon_name(icon));
    let text = account_name.map_or_else(
        || title.to_owned(),
        |name| {
            widgets
                .preferences
                .borrow()
                .favorite_name(email, label_id)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{title} · {name}"))
        },
    );
    content.append(
        &gtk::Label::builder()
            .label(&text)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    if account_name.is_some()
        && let Some(count) = state
            .borrow()
            .unread_counts
            .get(&(email.to_owned(), label_id.to_owned()))
            .copied()
        && count > 0
    {
        content.append(
            &gtk::Label::builder()
                .label(if count > 999 {
                    "999+".to_owned()
                } else {
                    count.to_string()
                })
                .tooltip_text(format!("{count} unread messages"))
                .css_classes(["postbird-unread-badge"])
                .build(),
        );
    }
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(&format!("{title} — {email}")));
    let widgets_for_select = widgets.clone();
    let state_for_select = state.clone();
    let selected_email = email.to_owned();
    let selected_label = label_id.to_owned();
    let selected_title = title.to_owned();
    button.connect_clicked(move |_| {
        select_mailbox(
            &widgets_for_select,
            &state_for_select,
            &selected_email,
            &selected_label,
            &selected_title,
        );
    });
    row.append(&button);

    if account_name.is_some() {
        let rename = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Rename favourite")
            .css_classes(["flat", "postbird-favorite"])
            .build();
        let widgets_for_rename = widgets.clone();
        let state_for_rename = state.clone();
        let rename_email = email.to_owned();
        let rename_label = label_id.to_owned();
        let rename_title = title.to_owned();
        rename.connect_clicked(move |_| {
            let widgets = widgets_for_rename.clone();
            let state = state_for_rename.clone();
            let email = rename_email.clone();
            let label_id = rename_label.clone();
            let title = rename_title.clone();
            glib::MainContext::default().spawn_local(async move {
                let name = gtk::Entry::builder()
                    .placeholder_text("Work, Personal")
                    .max_length(60)
                    .build();
                if let Some(current) = widgets.preferences.borrow().favorite_name(&email, &label_id) {
                    name.set_text(current);
                }
                let dialog = adw::AlertDialog::builder()
                    .heading("Rename favourite")
                    .body(format!("{title} in {email}. This changes only its name in Postbird; leave blank to use the original name."))
                    .extra_child(&name)
                    .close_response("cancel")
                    .default_response("save")
                    .build();
                dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
                if dialog.choose_future(Some(&widgets.window)).await != "save" {
                    return;
                }
                {
                    let mut preferences = widgets.preferences.borrow_mut();
                    preferences.set_favorite_name(&email, &label_id, name.text().as_str());
                    if let Err(error) = preferences.save() {
                        show_message(&widgets, &format!("Could not save favourite name: {error}"));
                    }
                }
                rebuild_sidebar(&widgets, &state);
            });
        });
        row.append(&rename);
    }

    let favorite = widgets.preferences.borrow().is_favorite(email, label_id);
    let star = gtk::Button::builder()
        .icon_name(if favorite {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        })
        .tooltip_text(if favorite {
            "Remove from Favourites"
        } else {
            "Add to Favourites"
        })
        .css_classes(["flat", "postbird-favorite"])
        .build();
    let widgets_for_favorite = widgets.clone();
    let state_for_favorite = state.clone();
    let favorite_email = email.to_owned();
    let favorite_label = label_id.to_owned();
    star.connect_clicked(move |_| {
        {
            let mut preferences = widgets_for_favorite.preferences.borrow_mut();
            preferences.toggle_favorite(&favorite_email, &favorite_label);
            if let Err(error) = preferences.save() {
                show_message(
                    &widgets_for_favorite,
                    &format!("Could not save Favourites: {error}"),
                );
            }
        }
        rebuild_sidebar(&widgets_for_favorite, &state_for_favorite);
        refresh_favorite_counts_for_account(
            &widgets_for_favorite,
            &state_for_favorite,
            &favorite_email,
        );
    });
    row.append(&star);
    row
}

fn rebuild_sidebar(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let navigation = &widgets.sidebar_navigation;
    while let Some(child) = navigation.first_child() {
        navigation.remove(&child);
    }
    let accounts = {
        let state = state.borrow();
        state
            .account_emails
            .iter()
            .enumerate()
            .map(|(index, email)| {
                (
                    email.clone(),
                    state
                        .account_names
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| email.clone()),
                    sidebar_folders(&state, email),
                    state.collapsed_accounts.contains(email),
                )
            })
            .collect::<Vec<_>>()
    };
    navigation.append(&sidebar_heading("FAVOURITES"));
    let mut any_favorites = false;
    for (email, name, folders, _) in &accounts {
        for (label_id, title, icon) in folders {
            if widgets.preferences.borrow().is_favorite(email, label_id) {
                any_favorites = true;
                navigation.append(&sidebar_folder_row(
                    widgets,
                    state,
                    email,
                    label_id,
                    title,
                    icon,
                    Some(name),
                ));
            }
        }
    }
    if !any_favorites {
        navigation.append(
            &gtk::Label::builder()
                .label("Star a folder to keep it here")
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    navigation.append(&gtk::Separator::new(Orientation::Horizontal));
    navigation.append(&sidebar_heading("ACCOUNTS"));
    for (email, name, folders, collapsed) in accounts {
        let folder_list = gtk::Box::new(Orientation::Vertical, 2);
        folder_list.add_css_class("postbird-account-folders");
        for (label_id, title, icon) in folders {
            folder_list.append(&sidebar_folder_row(
                widgets, state, &email, &label_id, &title, icon, None,
            ));
        }
        let account_label = gtk::Label::builder()
            .label(&name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let parent = gtk::Expander::builder()
            .label_widget(&account_label)
            .child(&folder_list)
            .expanded(!collapsed)
            .build();
        parent.add_css_class("postbird-account");
        let state_for_expand = state.clone();
        parent.connect_expanded_notify(move |expander| {
            let mut state = state_for_expand.borrow_mut();
            if expander.is_expanded() {
                state.collapsed_accounts.remove(&email);
            } else {
                state.collapsed_accounts.insert(email.clone());
            }
        });
        navigation.append(&parent);
    }
}

fn refresh_favorite_counts(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let emails = state.borrow().account_emails.clone();
    for email in emails {
        refresh_favorite_counts_for_account(widgets, state, &email);
    }
}

fn refresh_favorite_counts_for_account(widgets: &Widgets, state: &Rc<RefCell<State>>, email: &str) {
    let labels = {
        let state = state.borrow();
        if !state.account_emails.iter().any(|account| account == email) {
            return;
        }
        sidebar_folders(&state, email)
            .into_iter()
            .filter(|(label, _, _)| widgets.preferences.borrow().is_favorite(email, label))
            .map(|(label, _, _)| label)
            .collect::<Vec<_>>()
    };
    if labels.is_empty() {
        return;
    }
    let generation = {
        let mut state = state.borrow_mut();
        let generation = state
            .unread_refresh_generation
            .entry(email.to_owned())
            .or_default();
        *generation += 1;
        *generation
    };
    let widgets = widgets.clone();
    let state = state.clone();
    let email = email.to_owned();
    glib::MainContext::default().spawn_local(async move {
        let request_email = email.clone();
        let request_labels = labels.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<u32>> {
            MailClient::for_account(AccountStore::open()?, &request_email)?
                .unread_counts(&request_labels)
        })
        .await;
        let Ok(Ok(counts)) = result else {
            return;
        };
        if counts.len() != labels.len() {
            return;
        }
        let fresh_counts = labels
            .into_iter()
            .zip(counts)
            .filter(|(_, count)| *count > 0)
            .collect::<HashMap<_, _>>();
        {
            let mut state = state.borrow_mut();
            if state.unread_refresh_generation.get(&email) != Some(&generation)
                || !state.account_emails.contains(&email)
            {
                return;
            }
            let current_counts = state
                .unread_counts
                .iter()
                .filter(|((account, _), _)| account == &email)
                .map(|((_, label), count)| (label.clone(), *count))
                .collect::<HashMap<_, _>>();
            if current_counts == fresh_counts {
                return;
            }
            state
                .unread_counts
                .retain(|(account, _), _| account != &email);
            for (label, count) in fresh_counts {
                state.unread_counts.insert((email.clone(), label), count);
            }
        }
        rebuild_sidebar(&widgets, &state);
    });
}

fn select_mailbox(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    email: &str,
    label_id: &str,
    title: &str,
) {
    let Some(index) = state
        .borrow()
        .account_emails
        .iter()
        .position(|item| item == email)
    else {
        return;
    };
    state.borrow_mut().current_label = label_id.to_owned();
    widgets
        .mailbox_title
        .set_text(&format!("{title} · {email}"));
    widgets.search.set_text("");
    if widgets.account_picker.selected() != index as u32 {
        widgets.account_picker.set_selected(index as u32);
    } else {
        load_inbox(widgets, state, email.to_owned(), None, true);
    }
    rebuild_sidebar(widgets, state);
}

fn message_list_panel(widgets: &Widgets) -> gtk::Widget {
    let panel = gtk::Box::new(Orientation::Vertical, 5);
    panel.add_css_class("postbird-list-panel");
    panel.set_size_request(300, -1);
    panel.set_margin_top(6);
    panel.set_margin_bottom(6);
    panel.set_margin_start(6);
    panel.set_margin_end(6);
    let title_row = gtk::Box::new(Orientation::Horizontal, 6);
    widgets.mailbox_title.set_hexpand(true);
    title_row.append(&widgets.mailbox_title);
    title_row.append(&widgets.mailbox_spinner);
    panel.append(&title_row);
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
            state.borrow_mut().history_expanded = false;
            widgets.message_title.set_text(message.header("Subject"));
            widgets.message_sender.set_text(&format!(
                "{} message{} in this conversation",
                conversation.len(),
                if conversation.len() == 1 { "" } else { "s" }
            ));
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
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let refresh_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
            let mut client = MailClient::for_account(AccountStore::open()?, &email)?;
            for id in &message_ids {
                client.set_unread(id, false)?;
            }
            MailCache::open()?.mark_read(&email, &message_ids)
        })
        .await;
        match result {
            Ok(Ok(())) => refresh_favorite_counts_for_account(&widgets, &state, &refresh_email),
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
                let title = {
                    let state = state.borrow();
                    sidebar_folders(&state, &email)
                        .into_iter()
                        .find(|(id, _, _)| id == &state.current_label)
                        .map(|(_, title, _)| title)
                        .unwrap_or_else(|| "Mail".to_owned())
                };
                widgets
                    .mailbox_title
                    .set_text(&format!("{title} · {email}"));
                load_inbox(&widgets, &state, email, None, true);
                rebuild_sidebar(&widgets, &state);
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
            let choice = adw::AlertDialog::builder()
                .heading("Add an account")
                .body("Choose how Postbird connects to Gmail.")
                .close_response("cancel")
                .default_response("app-password")
                .build();
            choice.add_responses(&[
                ("cancel", "Cancel"),
                ("app-password", "Gmail app password"),
                ("goa", "GNOME Online Accounts"),
            ]);
            match choice.choose_future(Some(&widgets.window)).await.as_str() {
                "app-password" => add_app_password_account(&widgets, &state).await,
                "goa" => add_goa_account(&widgets, &state).await,
                _ => {}
            }
        });
    });
}

async fn add_app_password_account(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let address = gtk::Entry::builder()
        .placeholder_text("you@gmail.com")
        .input_purpose(gtk::InputPurpose::Email)
        .build();
    let password = gtk::PasswordEntry::builder()
        .placeholder_text("16-character app password")
        .show_peek_icon(true)
        .build();
    let form = gtk::Box::new(Orientation::Vertical, 12);
    form.append(&gtk::Label::builder()
        .label("Create an app password in your Google Account after enabling 2-Step Verification. Never enter your normal Google password.")
        .wrap(true)
        .xalign(0.0)
        .build());
    form.append(&address);
    form.append(&password);
    let dialog = adw::AlertDialog::builder()
        .heading("Gmail app password")
        .extra_child(&form)
        .close_response("cancel")
        .default_response("connect")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
    if dialog.choose_future(Some(&widgets.window)).await != "connect" {
        return;
    }
    let email = address.text().trim().to_owned();
    let secret = password.text().replace(' ', "");
    password.set_text("");
    if email.matches('@').count() != 1
        || email.chars().any(char::is_whitespace)
        || secret.is_empty()
    {
        return show_message(widgets, "Enter a full email address and app password");
    }
    show_message(widgets, "Checking Gmail IMAP and SMTP…");
    let result = gio::spawn_blocking(move || -> anyhow::Result<Account> {
        ImapClient::probe_credentials(&email, &secret)?;
        let store = AccountStore::open()?;
        let account = Account {
            display_name: email.clone(),
            email,
            kind: AccountKind::GmailAppPassword,
            goa_id: None,
        };
        store.save_app_password_account(account.clone(), &secret)?;
        Ok(account)
    })
    .await;
    match result {
        Ok(Ok(account)) => {
            load_accounts(widgets, state);
            if let Some(index) = account_index(state, &account.email) {
                widgets.account_picker.set_selected(index);
            }
        }
        Ok(Err(error)) => show_error(widgets, error),
        Err(_) => show_message(widgets, "The connection check stopped unexpectedly"),
    }
}

async fn add_goa_account(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let accounts = loop {
        let result = gio::spawn_blocking(ImapClient::goa_accounts).await;
        let issue = match result {
            Ok(Ok(accounts)) if !accounts.is_empty() => break accounts,
            Ok(Ok(_)) => GoaSetupIssue::NoMailAccount,
            Ok(Err(error)) => GoaSetupIssue::from_backend_error(&error.to_string()),
            Err(_) => {
                GoaSetupIssue::Unavailable("The Online Accounts check stopped unexpectedly".into())
            }
        };
        match goa_setup_dialog(widgets, &issue).await.as_str() {
            "retry" => continue,
            "app-password" => {
                add_app_password_account(widgets, state).await;
                return;
            }
            _ => return,
        }
    };
    let names = gtk::StringList::new(&[]);
    for account in &accounts {
        names.append(&account.email);
    }
    let picker = gtk::DropDown::builder().model(&names).build();
    let dialog = adw::AlertDialog::builder()
        .heading("Use a GNOME Online Account")
        .body("Choose a Google account with Mail enabled. Postbird will use GOA's sign-in and will not store its token.")
        .extra_child(&picker)
        .close_response("cancel")
        .default_response("connect")
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
    if dialog.choose_future(Some(&widgets.window)).await != "connect" {
        return;
    }
    let Some(selected) = accounts.get(picker.selected() as usize).cloned() else {
        return show_message(widgets, "Choose an Online Account");
    };
    show_message(widgets, "Checking GOA Gmail IMAP and SMTP…");
    let result = gio::spawn_blocking(move || -> anyhow::Result<Account> {
        ImapClient::probe_goa(&selected.email, &selected.id)?;
        let account = Account {
            display_name: format!("{} (GOA)", selected.email),
            email: selected.email,
            kind: AccountKind::GnomeOnlineAccounts,
            goa_id: Some(selected.id),
        };
        AccountStore::open()?.save_goa_account(account.clone())?;
        Ok(account)
    })
    .await;
    match result {
        Ok(Ok(account)) => {
            load_accounts(widgets, state);
            if let Some(index) = account_index(state, &account.email) {
                widgets.account_picker.set_selected(index);
            }
        }
        Ok(Err(error)) => show_error(widgets, error),
        Err(_) => show_message(widgets, "The Online Accounts check stopped unexpectedly"),
    }
}

enum GoaSetupIssue {
    MissingBindings,
    NoMailAccount,
    Unavailable(String),
}

impl GoaSetupIssue {
    fn from_backend_error(error: &str) -> Self {
        if error.contains("bindings are unavailable") {
            Self::MissingBindings
        } else {
            Self::Unavailable(error.to_owned())
        }
    }

    fn guidance(&self) -> (&'static str, String) {
        match self {
            Self::MissingBindings => (
                "GNOME Online Accounts is not ready",
                "Postbird needs GNOME Online Accounts and Python's GObject bindings. Install them with your distribution's package manager (on Arch: gnome-online-accounts and python-gobject). Then add a Google account in Online Accounts and enable Mail. You can use a Gmail app password instead.".into(),
            ),
            Self::NoMailAccount => (
                "No Google Mail account found",
                "Open Online Accounts, add your Google account, and enable Mail for it. If the account is already there, check that Mail is switched on. Then return to Postbird and try again. You can also use a Gmail app password.".into(),
            ),
            Self::Unavailable(error) => (
                "Could not connect to Online Accounts",
                format!("Check that Online Accounts opens and your Google account has Mail enabled, then retry. You can also use a Gmail app password.\n\nDetails: {error}"),
            ),
        }
    }
}

fn online_accounts_settings_command() -> Option<(PathBuf, &'static [&'static str])> {
    if let Some(path) = glib::find_program_in_path("gnome-online-accounts-gtk") {
        return Some((path, &[]));
    }
    glib::find_program_in_path("gnome-control-center")
        .map(|path| (path, &["online-accounts"] as &'static [&'static str]))
}

fn open_online_accounts_settings(path: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let mut command = vec![path.as_os_str()];
    command.extend(args.iter().map(OsStr::new));
    gio::Subprocess::newv(&command, gio::SubprocessFlags::NONE)?;
    Ok(())
}

async fn goa_setup_dialog(widgets: &Widgets, issue: &GoaSetupIssue) -> String {
    let (heading, body) = issue.guidance();
    let settings = online_accounts_settings_command();
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .close_response("close")
        .default_response("close")
        .build();
    dialog.add_responses(&[
        ("close", "Close"),
        ("app-password", "Use app password"),
        ("retry", "Retry"),
    ]);
    if settings.is_some() {
        dialog.add_response("open-settings", "Open Online Accounts");
    }
    let response = dialog.choose_future(Some(&widgets.window)).await;
    if response == "open-settings"
        && let Some((path, args)) = settings
    {
        match open_online_accounts_settings(&path, args) {
            Ok(()) => show_message(
                widgets,
                "Enable Mail in Online Accounts, then choose GOA in Postbird again",
            ),
            Err(error) => show_error(widgets, format!("Could not open Online Accounts: {error}")),
        }
    }
    response.to_string()
}

fn account_index(state: &Rc<RefCell<State>>, email: &str) -> Option<u32> {
    state
        .borrow()
        .account_emails
        .iter()
        .position(|candidate| candidate == email)
        .map(|index| index as u32)
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
                    "Remove {email} and its cached mail from Postbird? Any Postbird-stored sign-in secret will be removed. This does not remove a GNOME Online Account or delete Gmail messages."
                ))
                .close_response("cancel")
                .default_response("cancel")
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove Account")]);
            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            if dialog.choose_future(Some(&widgets.window)).await != "remove" {
                return;
            }
            let removing_email = email.clone();
            let result = gio::spawn_blocking(move || {
                AccountStore::open()?.remove_account(&removing_email)
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    {
                        let mut state = state.borrow_mut();
                        state.load_generation += 1;
                        state.load_cancel.store(true, Ordering::Relaxed);
                        state.pending_archives.retain(|(account, _)| account != &email);
                        state.archived_messages.retain(|(account, _), _| account != &email);
                        state.current_label = "INBOX".to_owned();
                    }
                    reset_reader(&widgets, &state);
                    widgets.search.set_text("");
                    {
                        let mut preferences = widgets.preferences.borrow_mut();
                        preferences.remove_account(&email);
                        if let Err(error) = preferences.save() {
                            show_message(&widgets, &format!("Could not save Favourites: {error}"));
                        }
                    }
                    load_accounts(&widgets, &state);
                }
                Ok(Err(error)) => show_error(&widgets, error),
                Err(_) => show_message(&widgets, "The account removal task stopped unexpectedly"),
            }
        });
    });
}

fn connect_archive(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    widgets.archive.clone().connect_clicked(move |_| {
        let (email, conversation, label) = {
            let state = state.borrow();
            let Some(email) = state
                .account_emails
                .get(widgets.account_picker.selected() as usize)
                .cloned()
            else {
                return;
            };
            (
                email,
                state.selected_conversation.clone(),
                state.current_label.clone(),
            )
        };
        let Some(first) = conversation.first() else {
            return;
        };
        let thread_id = first.thread_id.clone();
        let key = (email.clone(), thread_id.clone());
        if !state.borrow_mut().pending_archives.insert(key.clone()) {
            return;
        }
        cancel_active_refresh(&widgets, &state);
        let query = widgets.search.text().to_string();
        remove_conversation(&widgets, &state, &thread_id);
        update_mailbox_spinner(&widgets, &state);
        let widgets = widgets.clone();
        let state = state.clone();
        // Closing the last window must not discard an archive already queued.
        let hold = widgets.window.application().map(|app| app.hold());
        glib::MainContext::default().spawn_local(async move {
            let _hold = hold;
            let request_email = email.clone();
            let request_thread = thread_id.clone();
            let message_ids = conversation
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>();
            let archived_ids = message_ids.iter().cloned().collect::<HashSet<_>>();
            let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                MailClient::for_account(AccountStore::open()?, &request_email)?
                    .archive_thread(&request_thread)?;
                // A cache failure must not roll back a successful Gmail archive.
                if let Err(error) = MailCache::open()
                    .and_then(|mut cache| cache.remove_messages(&request_email, &message_ids))
                {
                    eprintln!("Could not update the archive cache: {error}");
                }
                Ok(())
            })
            .await;
            state.borrow_mut().pending_archives.remove(&key);
            let succeeded = matches!(result, Ok(Ok(())));
            if succeeded {
                state
                    .borrow_mut()
                    .archived_messages
                    .retain(|_, (_, completed)| {
                        completed.elapsed() < std::time::Duration::from_secs(60)
                    });
                state
                    .borrow_mut()
                    .archived_messages
                    .insert(key, (archived_ids, std::time::Instant::now()));
            } else {
                let same_view = {
                    let state = state.borrow();
                    state
                        .account_emails
                        .get(widgets.account_picker.selected() as usize)
                        == Some(&email)
                        && state.current_label == label
                        && widgets.search.text().as_str() == query
                };
                if same_view {
                    restore_archived_conversation(&widgets, &state, conversation);
                }
                let detail = match result {
                    Ok(Err(error)) => error.to_string(),
                    _ => "The background task stopped unexpectedly.".to_owned(),
                };
                show_message(
                    &widgets,
                    &format!(
                        "Could not archive the conversation. {} {detail}",
                        if same_view {
                            "It has been restored to the list."
                        } else {
                            "It remains in Gmail."
                        }
                    ),
                );
            }
            update_mailbox_spinner(&widgets, &state);
            if succeeded {
                refresh_favorite_counts_for_account(&widgets, &state, &email);
            }
        });
    });
}

fn restore_archived_conversation(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    conversation: Vec<Message>,
) {
    let selected = state
        .borrow()
        .selected
        .as_ref()
        .map(|message| message.thread_id.clone());
    let restored_id = conversation
        .first()
        .map(|message| message.thread_id.clone());
    let mut messages = state
        .borrow()
        .conversations
        .iter()
        .flatten()
        .filter(|message| Some(&message.thread_id) != restored_id.as_ref())
        .cloned()
        .collect::<Vec<_>>();
    messages.extend(conversation);
    display_messages(widgets, state, messages);
    let target = selected.or(restored_id);
    let index = state
        .borrow()
        .conversations
        .iter()
        .position(|conversation| {
            conversation
                .first()
                .is_some_and(|message| Some(&message.thread_id) == target.as_ref())
        });
    if let Some(row) = index.and_then(|index| widgets.messages.row_at_index(index as i32)) {
        widgets.messages.select_row(Some(&row));
    }
}

fn connect_message_actions(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    connect_archive(widgets, state);
    for (button, action) in [
        (widgets.star.clone(), "star"),
        (widgets.trash.clone(), "trash"),
        (widgets.unread.clone(), "unread"),
    ] {
        let widgets = widgets.clone();
        let state = state.clone();
        button.connect_clicked(move |_| {
            let Some(message) = state.borrow().selected.clone() else {
                return;
            };
            let conversation = state.borrow().selected_conversation.clone();
            let index = widgets.account_picker.selected() as usize;
            let Some(email) = state.borrow().account_emails.get(index).cloned() else {
                return;
            };
            let action = action.to_owned();
            let action_for_request = action.clone();
            let message_id = message.id.clone();
            let thread_id = message.thread_id.clone();
            let message_ids = conversation
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>();
            let starred = !message.label_ids.iter().any(|label| label == "STARRED");
            let label = state.borrow().current_label.clone();
            let query = widgets.search.text().to_string();
            cancel_active_refresh(&widgets, &state);
            let widgets_async = widgets.clone();
            let state_async = state.clone();
            glib::MainContext::default().spawn_local(async move {
                let request_email = email.clone();
                let request_ids = message_ids.clone();
                let request_id = message_id.clone();
                let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                    let mut client =
                        MailClient::for_account(AccountStore::open()?, &request_email)?;
                    match action_for_request.as_str() {
                        "star" => client.set_starred(&request_id, starred)?,
                        "trash" => {
                            for id in &request_ids {
                                client.trash(id)?;
                            }
                        }
                        "unread" => {
                            for id in &request_ids {
                                client.set_unread(id, true)?;
                            }
                        }
                        _ => {}
                    }
                    let cache_result =
                        MailCache::open().and_then(|mut cache| match action_for_request.as_str() {
                            "star" => {
                                cache.set_label(&request_email, &[request_id], "STARRED", starred)
                            }
                            "trash" => {
                                cache.remove_messages_everywhere(&request_email, &request_ids)
                            }
                            "unread" => {
                                cache.set_label(&request_email, &request_ids, "UNREAD", true)
                            }
                            _ => Ok(()),
                        });
                    if let Err(error) = cache_result {
                        eprintln!("Could not update the message cache: {error}");
                    }
                    Ok(())
                })
                .await;
                match result {
                    Ok(Ok(())) => {
                        refresh_favorite_counts_for_account(&widgets_async, &state_async, &email);
                        let account_index = widgets_async.account_picker.selected() as usize;
                        let same_account =
                            state_async.borrow().account_emails.get(account_index) == Some(&email);
                        let same_view = same_account
                            && state_async.borrow().current_label == label
                            && widgets_async.search.text().as_str() == query;
                        if same_view {
                            match action.as_str() {
                                "star" => {
                                    let ids = HashSet::from([message_id]);
                                    update_visible_labels(&state_async, &ids, "STARRED", starred);
                                    let no_longer_starred = label == "STARRED"
                                        && state_async
                                            .borrow()
                                            .conversations
                                            .iter()
                                            .find(|conversation| {
                                                conversation
                                                    .first()
                                                    .is_some_and(|item| item.thread_id == thread_id)
                                            })
                                            .is_some_and(|conversation| {
                                                conversation.iter().all(|item| {
                                                    !item.label_ids.contains(&"STARRED".to_owned())
                                                })
                                            });
                                    if no_longer_starred {
                                        remove_conversation(
                                            &widgets_async,
                                            &state_async,
                                            &thread_id,
                                        );
                                    } else {
                                        update_conversation_row(
                                            &widgets_async,
                                            &state_async,
                                            &thread_id,
                                        );
                                    }
                                }
                                "trash" => {
                                    remove_conversation(&widgets_async, &state_async, &thread_id);
                                }
                                "unread" => {
                                    let ids = message_ids.into_iter().collect::<HashSet<_>>();
                                    update_visible_labels(&state_async, &ids, "UNREAD", true);
                                    update_conversation_row(
                                        &widgets_async,
                                        &state_async,
                                        &thread_id,
                                    );
                                    clear_reader_view(&widgets_async, &state_async);
                                }
                                _ => {}
                            }
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
            return show_message(&widgets, "Connect a mail account before composing");
        };
        present_compose(&widgets, email, None, None, None);
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
            forwarded_attachments: Vec::new(),
        };
        present_compose(&widgets, email, Some(reply), Some(original), None);
    });
}

fn forward_message(original: &Message) -> ComposeMessage {
    let subject = original.header("Subject");
    ComposeMessage {
        to: String::new(),
        cc: String::new(),
        bcc: String::new(),
        subject: if subject.to_ascii_lowercase().starts_with("fwd:") {
            subject.to_owned()
        } else {
            format!("Fwd: {subject}")
        },
        body: format!(
            "\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n{}\n{}",
            original.header("From"),
            original.header("Date"),
            subject,
            original.header("To"),
            if original.header("Cc").is_empty() {
                String::new()
            } else {
                format!("Cc: {}\n", original.header("Cc"))
            },
            original.body_text()
        ),
        html_body: None,
        in_reply_to: None,
        thread_id: None,
        attachments: Vec::new(),
        forwarded_attachments: Vec::new(),
    }
}

fn connect_forward(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    widgets.forward.clone().connect_clicked(move |button| {
        let Some(email) = state
            .borrow()
            .account_emails
            .get(widgets.account_picker.selected() as usize)
            .cloned()
        else {
            return;
        };
        let Some(original) = state.borrow().selected.clone() else {
            return;
        };
        let widgets = widgets.clone();
        let button = button.clone();
        button.set_sensitive(false);
        if !original.attachments().is_empty() {
            show_message(&widgets, "Preparing forwarded attachments…");
        }
        glib::MainContext::default().spawn_local(async move {
            let source_email = email.clone();
            let result = gio::spawn_blocking(move || -> anyhow::Result<ComposeMessage> {
                let mut forward = forward_message(&original);
                let mut client = None;
                for part in original.attachments() {
                    let data = if let Some(data) = &part.body.data {
                        crate::gmail::decode_attachment_data(data)?
                    } else {
                        if client.is_none() {
                            client = Some(MailClient::for_account(
                                AccountStore::open()?,
                                &source_email,
                            )?);
                        }
                        client
                            .as_mut()
                            .unwrap()
                            .attachment(&original.id, &part.body)?
                    };
                    forward.forwarded_attachments.push(ForwardedAttachment {
                        content_id: None,
                        filename: attachment_filename(part),
                        mime_type: part.mime_type.clone(),
                        data: data.into(),
                    });
                }
                Ok(forward)
            })
            .await;
            button.set_sensitive(true);
            match result {
                Ok(Ok(forward)) => present_compose(&widgets, email, Some(forward), None, None),
                Ok(Err(error)) => {
                    show_message(&widgets, &format!("Could not prepare forward: {error}"))
                }
                Err(_) => show_message(&widgets, "The forward preparation stopped unexpectedly"),
            }
        });
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

#[derive(Clone)]
struct EditingDraft {
    id: String,
    message_id: String,
    account_email: String,
    state: Rc<RefCell<State>>,
}

fn draft_compose_message(message: &Message) -> anyhow::Result<ComposeMessage> {
    let mut attachments = Vec::new();
    for part in message.attachments() {
        let data = part
            .body
            .data
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Draft attachment was not downloaded"))?;
        attachments.push(ForwardedAttachment {
            filename: attachment_filename(part),
            mime_type: part.mime_type.clone(),
            data: crate::gmail::decode_attachment_data(data)?.into(),
            content_id: part
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case("Content-ID"))
                .map(|header| header.value.trim().trim_matches(['<', '>']).to_owned()),
        });
    }
    Ok(ComposeMessage {
        to: message.header("To").to_owned(),
        cc: message.header("Cc").to_owned(),
        bcc: message.header("Bcc").to_owned(),
        subject: message.header("Subject").to_owned(),
        body: message.body_text(),
        html_body: message.body_html(),
        in_reply_to: (!message.header("In-Reply-To").is_empty())
            .then(|| message.header("In-Reply-To").to_owned()),
        thread_id: Some(message.thread_id.clone()),
        attachments: Vec::new(),
        forwarded_attachments: attachments,
    })
}

fn edit_draft(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    message: &Message,
    button: &gtk::Button,
) {
    let Some(email) = state
        .borrow()
        .account_emails
        .get(widgets.account_picker.selected() as usize)
        .cloned()
    else {
        return;
    };
    let widgets = widgets.clone();
    let state = state.clone();
    let message_id = message.id.clone();
    let button = button.clone();
    button.set_sensitive(false);
    glib::MainContext::default().spawn_local(async move {
        let source_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<_> {
            let draft = MailClient::for_account(AccountStore::open()?, &source_email)?
                .draft_for_message(&message_id)?;
            let compose = draft_compose_message(&draft.message)?;
            Ok((draft, compose))
        })
        .await;
        button.set_sensitive(true);
        match result {
            Ok(Ok((draft, compose))) => {
                let editing = EditingDraft {
                    id: draft.id,
                    message_id: draft.message.id,
                    account_email: email.clone(),
                    state,
                };
                present_compose(&widgets, email, Some(compose), None, Some(editing));
            }
            Ok(Err(error)) => show_error(&widgets, error),
            Err(_) => show_message(&widgets, "Could not open the draft. Please try again."),
        }
    });
}

fn refresh_after_draft(widgets: &Widgets, editing: &Option<EditingDraft>) {
    if let Some(editing) = editing {
        let selected_email = editing
            .state
            .borrow()
            .account_emails
            .get(widgets.account_picker.selected() as usize)
            .cloned();
        if selected_email.as_deref() == Some(editing.account_email.as_str()) {
            let query = widgets.search.text().to_string();
            load_inbox(
                widgets,
                &editing.state,
                editing.account_email.clone(),
                (!query.trim().is_empty()).then_some(query),
                false,
            );
        }
    }
}

fn present_compose(
    widgets: &Widgets,
    account_email: String,
    initial: Option<ComposeMessage>,
    replying_to: Option<Message>,
    editing: Option<EditingDraft>,
) {
    let parent = widgets.window.clone();
    let dialog = adw::Dialog::builder()
        .title(if editing.is_some() {
            "Edit Draft"
        } else if replying_to.is_some() {
            "Reply"
        } else if initial.is_some() {
            "Forward"
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
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_wait = gtk::Button::builder()
        .label("Cancel wait")
        .visible(false)
        .build();
    let cancel_flag = cancelled.clone();
    cancel_wait.connect_clicked(move |_| cancel_flag.store(true, Ordering::Relaxed));
    header.pack_end(&cancel_wait);
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
    let forwarded_attachments = initial
        .as_ref()
        .map(|message| message.forwarded_attachments.clone())
        .unwrap_or_default();
    let forwarded_names = forwarded_attachments
        .iter()
        .map(|attachment| attachment.filename.clone())
        .collect::<Vec<_>>();
    let attachment_paths = Rc::new(RefCell::new(
        initial
            .as_ref()
            .map(|message| message.attachments.clone())
            .unwrap_or_default(),
    ));
    let attachment_label = gtk::Label::builder()
        .halign(Align::Start)
        .css_classes(["dim-label"])
        .build();
    if !forwarded_names.is_empty() {
        attachment_label.set_text(&format!("Attached: {}", forwarded_names.join(", ")));
    }
    attachment_label.set_wrap(true);
    attachment_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    let parent_for_attachment = parent.clone();
    let paths_for_attachment = attachment_paths.clone();
    let label_for_attachment = attachment_label.clone();
    attach.connect_clicked(move |_| {
        let parent = parent_for_attachment.clone();
        let paths = paths_for_attachment.clone();
        let label = label_for_attachment.clone();
        let forwarded_names = forwarded_names.clone();
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
                            .map(|name| name.to_string_lossy().into_owned())
                            .chain(forwarded_names)
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
    if editing.is_some()
        && initial
            .as_ref()
            .is_some_and(|message| message.html_body.is_some())
    {
        let note = gtk::Label::builder()
            .label("Editing the body replaces its original formatting with Postbird formatting. Changing only recipients or the subject preserves the original HTML.")
            .wrap(true).xalign(0.0).css_classes(["caption", "dim-label"]).build();
        form.append(&note);
    }
    let body_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&body)
        .build();
    form.append(&body_scroll);
    if let Some(message) = replying_to.as_ref() {
        form.append(&reply_context(message));
    }
    toolbar.set_content(Some(&form));
    dialog.set_child(Some(&toolbar));

    let original_content = rich_text_content(&body);
    let original_html = editing
        .as_ref()
        .and_then(|_| initial.as_ref()?.html_body.clone());
    let content_for_send = original_content.clone();
    let html_for_send = original_html.clone();
    let editing_for_send = editing.clone();
    let busy_controls: Vec<gtk::Widget> = vec![
        form.clone().upcast(),
        send.clone().upcast(),
        save_draft.clone().upcast(),
        attach.clone().upcast(),
    ];
    let busy_controls = busy_controls
        .iter()
        .map(|control| control.downgrade())
        .collect::<Vec<_>>();
    let progress = gtk::Label::builder()
        .label("Working… Gmail requests may wait briefly before continuing.")
        .wrap(true)
        .visible(false)
        .css_classes(["caption", "dim-label"])
        .build();
    form.prepend(&progress);
    let progress = progress.downgrade();
    let cancel_wait = cancel_wait.downgrade();
    let busy_dialog = dialog.downgrade();
    let set_busy: Rc<dyn Fn(bool)> = Rc::new(move |busy| {
        if let Some(cancel_wait) = cancel_wait.upgrade() {
            cancel_wait.set_visible(busy);
        }
        if let Some(progress) = progress.upgrade() {
            progress.set_visible(busy);
        }
        for control in &busy_controls {
            if let Some(control) = control.upgrade() {
                control.set_sensitive(!busy);
            }
        }
        if let Some(dialog) = busy_dialog.upgrade() {
            dialog.set_can_close(!busy);
        }
    });
    let busy_send = set_busy.clone();
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
    let forwarded_send = forwarded_attachments.clone();
    let cancel_send = cancelled.clone();
    send.connect_clicked(move |_| {
        cancel_send.store(false, Ordering::Relaxed);
        let cancelled = cancel_send.clone();
        busy_send(true);
        let editing = editing_for_send.clone();
        let (plain_body, html_body) =
            compose_body_content(&body_send, &content_for_send, html_for_send.as_deref());
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
            forwarded_attachments: forwarded_send.clone(),
        };
        let widgets = widgets_for_send.clone();
        let dialog = dialog_for_send.clone();
        let account_email = account_for_send.clone();
        let set_busy = busy_send.clone();
        glib::MainContext::default().spawn_local(async move {
            let target = editing
                .as_ref()
                .map(|draft| (draft.id.clone(), draft.message_id.clone()));
            let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                let mut client = MailClient::for_account(AccountStore::open()?, &account_email)?;
                client.set_cancellation(Some(cancelled));
                if let Some(target) = target {
                    client.write_existing_draft(&target.0, &target.1, &message, true)?;
                } else {
                    client.send(&message)?;
                }
                Ok(())
            })
            .await;
            set_busy(false);
            match result {
                Ok(Ok(())) => {
                    dialog.close();
                    show_message(&widgets, "Message sent");
                    refresh_after_draft(&widgets, &editing);
                }
                Ok(Err(error)) => {
                    show_error(&widgets, error);
                }
                Err(_) => {
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
    save_draft.connect_clicked(move |_| {
        cancelled.store(false, Ordering::Relaxed);
        let cancelled = cancelled.clone();
        set_busy(true);
        let editing = editing.clone();
        let (plain_body, html_body) =
            compose_body_content(&body, &original_content, original_html.as_deref());
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
            forwarded_attachments: forwarded_attachments.clone(),
        };
        let widgets = widgets_for_draft.clone();
        let dialog = dialog_for_draft.clone();
        let account_email = account_for_draft.clone();
        let set_busy = set_busy.clone();
        glib::MainContext::default().spawn_local(async move {
            let target = editing
                .as_ref()
                .map(|draft| (draft.id.clone(), draft.message_id.clone()));
            let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                let mut client = MailClient::for_account(AccountStore::open()?, &account_email)?;
                client.set_cancellation(Some(cancelled));
                if let Some(target) = target {
                    client.write_existing_draft(&target.0, &target.1, &message, false)?;
                } else {
                    client.create_draft(&message)?;
                }
                Ok(())
            })
            .await;
            set_busy(false);
            match result {
                Ok(Ok(())) => {
                    dialog.close();
                    show_message(&widgets, "Draft saved");
                    refresh_after_draft(&widgets, &editing);
                }
                Ok(Err(error)) => {
                    show_error(&widgets, error);
                }
                Err(_) => {
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

fn compose_body_content(
    editor: &gtk::TextView,
    original: &(String, String),
    original_html: Option<&str>,
) -> (String, String) {
    let content = rich_text_content(editor);
    if &content == original
        && let Some(html) = original_html
    {
        (content.0, html.to_owned())
    } else {
        content
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
    {
        let mut state = state.borrow_mut();
        state.account_emails.clear();
        state.account_names.clear();
        state.account_labels.clear();
        state.current_label = "INBOX".to_owned();
    }
    for account in accounts {
        widgets.accounts.append(&account.display_name);
        let mut state = state.borrow_mut();
        state.account_names.push(account.display_name);
        state.account_emails.push(account.email);
    }
    {
        let mut state = state.borrow_mut();
        let accounts = state.account_emails.iter().cloned().collect::<HashSet<_>>();
        state
            .unread_counts
            .retain(|(email, _), _| accounts.contains(email));
    }
    let has_accounts = !state.borrow().account_emails.is_empty();
    widgets.account_picker.set_sensitive(has_accounts);
    if has_accounts {
        widgets.account_picker.set_selected(0);
        let email = state.borrow().account_emails[0].clone();
        widgets.mailbox_title.set_text(&format!("Inbox · {email}"));
        load_inbox(widgets, state, email, None, false);
        for email in state.borrow().account_emails.clone() {
            load_labels(widgets, state, email);
        }
    } else {
        clear_list(&widgets.messages);
        add_status_row(
            &widgets.messages,
            "No account connected",
            "Use the + button to add a Gmail account.",
        );
    }
    rebuild_sidebar(widgets, state);
    refresh_favorite_counts(widgets, state);
}

fn load_labels(widgets: &Widgets, state: &Rc<RefCell<State>>, email: String) {
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let request_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<crate::gmail::Label>> {
            MailClient::for_account(AccountStore::open()?, &request_email)?.labels()
        })
        .await;
        if !state.borrow().account_emails.contains(&email) {
            return;
        }
        let mut labels = match result {
            Ok(Ok(labels)) => labels
                .into_iter()
                .filter(|label| label.kind.eq_ignore_ascii_case("user"))
                .collect::<Vec<_>>(),
            Ok(Err(error)) => {
                return show_message(&widgets, &format!("Could not load labels: {error}"));
            }
            Err(_) => {
                return show_message(&widgets, "The label task stopped unexpectedly");
            }
        };
        labels.sort_by_key(|label| label.name.to_lowercase());
        let has_favorite_custom_label = labels
            .iter()
            .any(|label| widgets.preferences.borrow().is_favorite(&email, &label.id));
        state.borrow_mut().account_labels.insert(
            email.clone(),
            labels
                .into_iter()
                .map(|label| (label.id, label.name))
                .collect(),
        );
        rebuild_sidebar(&widgets, &state);
        if has_favorite_custom_label {
            refresh_favorite_counts_for_account(&widgets, &state, &email);
        }
    });
}

fn update_mailbox_spinner(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let state = state.borrow();
    let active_account = state
        .account_emails
        .get(widgets.account_picker.selected() as usize);
    let archiving = state
        .pending_archives
        .iter()
        .any(|(email, _)| Some(email) == active_account);
    let busy = state.mailbox_loading || archiving;
    widgets.mailbox_spinner.set_tooltip_text(Some(if archiving {
        "Archiving conversations…"
    } else {
        "Syncing this folder…"
    }));
    widgets.mailbox_spinner.set_spinning(busy);
    widgets.mailbox_spinner.set_visible(busy);
}

fn cancel_active_refresh(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    {
        let mut state = state.borrow_mut();
        state.load_generation += 1;
        state.load_cancel.store(true, Ordering::Relaxed);
        state.mailbox_loading = false;
        if let Some(cancelled) = &state.inbox_sync_cancel {
            cancelled.store(true, Ordering::Relaxed);
        }
    }
    update_mailbox_spinner(widgets, state);
}

fn sync_recent_inbox(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let (email, cancelled) = {
        let mut state = state.borrow_mut();
        if state.inbox_sync_account.is_some()
            || state.mailbox_loading
            || state.current_label != "INBOX"
            || !widgets.search.text().trim().is_empty()
        {
            return;
        }
        let index = widgets.account_picker.selected() as usize;
        let Some(email) = state.account_emails.get(index).cloned() else {
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        state.inbox_sync_account = Some(email.clone());
        state.inbox_sync_cancel = Some(cancelled.clone());
        (email, cancelled)
    };
    update_mailbox_spinner(widgets, state);
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let sync_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<Message>> {
            let mut client = MailClient::for_account(AccountStore::open()?, &sync_email)?;
            client.set_cancellation(Some(cancelled.clone()));
            let page = client.list_threads_with_limit(Some("INBOX"), None, None, 50)?;
            let cached = MailCache::open()?.messages(&sync_email, "INBOX")?;
            let needed = references_to_refresh(&page.threads, &cached, 3);
            let refreshed =
                fetch_threads_parallel(&sync_email, needed.clone(), Some(cancelled.clone()))?;
            let combined = merge_refreshed_threads(cached, &page.threads, &needed, refreshed);
            if cancelled.load(Ordering::Relaxed) {
                anyhow::bail!("Request cancelled");
            }
            AccountStore::open()?.account(&sync_email)?;
            MailCache::open()?.replace_mailbox(&sync_email, "INBOX", &combined)?;
            Ok(combined)
        })
        .await;
        state.borrow_mut().inbox_sync_account = None;
        state.borrow_mut().inbox_sync_cancel = None;
        update_mailbox_spinner(&widgets, &state);
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

fn references_to_refresh(
    references: &[ThreadRef],
    cached: &[Message],
    recent_count: usize,
) -> Vec<ThreadRef> {
    let cached_threads = cached
        .iter()
        .map(|message| message.thread_id.as_str())
        .collect::<HashSet<_>>();
    references
        .iter()
        .enumerate()
        .filter(|(index, reference)| {
            *index < recent_count || !cached_threads.contains(reference.id.as_str())
        })
        .map(|(_, reference)| reference.clone())
        .collect()
}

fn merge_refreshed_threads(
    mut cached: Vec<Message>,
    references: &[ThreadRef],
    refreshed_references: &[ThreadRef],
    refreshed: Vec<Message>,
) -> Vec<Message> {
    let current_threads = references
        .iter()
        .map(|reference| reference.id.as_str())
        .collect::<HashSet<_>>();
    let updated_threads = refreshed_references
        .iter()
        .map(|reference| reference.id.as_str())
        .collect::<HashSet<_>>();
    cached.retain(|message| {
        current_threads.contains(message.thread_id.as_str())
            && !updated_threads.contains(message.thread_id.as_str())
    });
    cached.extend(refreshed);
    cached.sort_by_key(|message| std::cmp::Reverse(message_timestamp(message)));
    cached
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
        "Fetching messages from Gmail…",
    );
    let widgets = widgets.clone();
    let state = state.clone();
    let (label, generation, cancelled) = {
        let mut state = state.borrow_mut();
        state.load_generation += 1;
        state.load_cancel.store(true, Ordering::Relaxed);
        state.load_cancel = Arc::new(AtomicBool::new(false));
        state.mailbox_loading = true;
        (
            state.current_label.clone(),
            state.load_generation,
            state.load_cancel.clone(),
        )
    };
    update_mailbox_spinner(&widgets, &state);
    let cached_mailbox = query.is_none().then(|| label.clone());
    glib::MainContext::default().spawn_local(async move {
        let mut showing_cached_messages = false;
        let mut cached_messages = Vec::new();
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
                cached_messages = messages.clone();
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
            let mut client = MailClient::for_account(AccountStore::open()?, &online_email)?;
            client.set_cancellation(Some(cancelled.clone()));
            let label = (!label.is_empty()).then_some(label.as_str());
            let page = client.list_threads(label, query.as_deref(), None)?;
            let needed = references_to_refresh(&page.threads, &cached_messages, 3);
            let refreshed =
                fetch_threads_parallel(&online_email, needed.clone(), Some(cancelled.clone()))?;
            let messages =
                merge_refreshed_threads(cached_messages, &page.threads, &needed, refreshed);
            if let Some(mailbox) = mailbox_to_cache {
                if cancelled.load(Ordering::Relaxed) {
                    anyhow::bail!("Request cancelled");
                }
                AccountStore::open()?.account(&online_email)?;
                MailCache::open()?.replace_mailbox(&cache_email, &mailbox, &messages)?;
            }
            Ok(messages)
        })
        .await;
        if state.borrow().load_generation != generation {
            return;
        }
        state.borrow_mut().mailbox_loading = false;
        update_mailbox_spinner(&widgets, &state);
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
    state.borrow().load_cancel.store(true, Ordering::Relaxed);
    state.borrow_mut().conversations.clear();
    state.borrow_mut().mailbox_loading = false;
    update_mailbox_spinner(widgets, state);
    clear_reader_view(widgets, state);
}

fn clear_reader_view(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    widgets.messages.unselect_all();
    {
        let mut state = state.borrow_mut();
        state.selected = None;
        state.selected_conversation.clear();
        state.history_expanded = false;
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

fn set_labels_for_messages(
    messages: &mut [Message],
    ids: &HashSet<String>,
    label: &str,
    enabled: bool,
) {
    for message in messages
        .iter_mut()
        .filter(|message| ids.contains(&message.id))
    {
        if enabled {
            if !message.label_ids.iter().any(|existing| existing == label) {
                message.label_ids.push(label.to_owned());
            }
        } else {
            message.label_ids.retain(|existing| existing != label);
        }
    }
}

fn update_visible_labels(
    state: &Rc<RefCell<State>>,
    ids: &HashSet<String>,
    label: &str,
    enabled: bool,
) {
    let mut state = state.borrow_mut();
    for conversation in &mut state.conversations {
        set_labels_for_messages(conversation, ids, label, enabled);
    }
    set_labels_for_messages(&mut state.selected_conversation, ids, label, enabled);
    if let Some(selected) = &mut state.selected {
        set_labels_for_messages(std::slice::from_mut(selected), ids, label, enabled);
    }
}

fn update_conversation_row(widgets: &Widgets, state: &Rc<RefCell<State>>, thread_id: &str) {
    let state = state.borrow();
    let Some((index, conversation)) =
        state
            .conversations
            .iter()
            .enumerate()
            .find(|(_, messages)| {
                messages
                    .first()
                    .is_some_and(|message| message.thread_id == thread_id)
            })
    else {
        return;
    };
    let Some(message) = conversation.first() else {
        return;
    };
    let Some(row) = widgets.messages.row_at_index(index as i32) else {
        return;
    };
    let starred = conversation
        .iter()
        .any(|message| message.label_ids.contains(&"STARRED".to_owned()));
    let has_draft = conversation
        .iter()
        .any(|message| message.label_ids.contains(&"DRAFT".to_owned()));
    row.set_child(Some(&conversation_row_content(
        message,
        conversation.len(),
        starred,
        has_draft,
    )));
    row.remove_css_class("accent");
    if conversation
        .iter()
        .any(|message| message.label_ids.contains(&"UNREAD".to_owned()))
    {
        row.add_css_class("accent");
    }
}

fn fetch_threads_parallel(
    email: &str,
    references: Vec<crate::gmail::ThreadRef>,
    cancelled: Option<Arc<AtomicBool>>,
) -> anyhow::Result<Vec<Message>> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let worker_count = references.len().min(4);
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
                let cancelled = cancelled.clone();
                scope.spawn(move || -> anyhow::Result<Vec<_>> {
                    let mut client = MailClient::for_account(AccountStore::open()?, &email)?;
                    client.set_cancellation(cancelled);
                    let references = batch
                        .iter()
                        .map(|(_, reference)| reference.clone())
                        .collect::<Vec<_>>();
                    let threads = client.threads(&references)?;
                    if threads.len() != batch.len() {
                        anyhow::bail!("mail server returned an incomplete thread batch");
                    }
                    Ok(batch
                        .into_iter()
                        .zip(threads)
                        .map(|((index, _), thread)| (index, thread.messages))
                        .collect())
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

fn archive_is_hidden(state: &State, email: &str, conversation: &[Message]) -> bool {
    let Some(first) = conversation.first() else {
        return false;
    };
    let key = (email.to_owned(), first.thread_id.clone());
    state.pending_archives.contains(&key)
        || (state.current_label == "INBOX"
            && state
                .archived_messages
                .get(&key)
                .is_some_and(|(ids, completed)| {
                    completed.elapsed() < std::time::Duration::from_secs(60)
                        && conversation.iter().all(|message| ids.contains(&message.id))
                }))
}

fn display_messages(widgets: &Widgets, state: &Rc<RefCell<State>>, messages: Vec<Message>) {
    clear_list(&widgets.messages);
    let mut conversations = group_conversations(messages);
    {
        let state = state.borrow();
        if let Some(email) = state
            .account_emails
            .get(widgets.account_picker.selected() as usize)
        {
            conversations.retain(|conversation| !archive_is_hidden(&state, email, conversation));
        }
    }
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
        let has_draft = conversation
            .iter()
            .any(|message| message.label_ids.iter().any(|label| label == "DRAFT"));
        let row = conversation_list_row(message, conversation.len(), starred, has_draft);
        if conversation
            .iter()
            .any(|message| message.label_ids.iter().any(|label| label == "UNREAD"))
        {
            row.add_css_class("accent");
        }
        widgets.messages.append(&row);
    }
}

fn draft_indicator() -> gtk::Label {
    gtk::Label::builder()
        .label("Draft")
        .css_classes(["error", "heading"])
        .tooltip_text("Unsent draft")
        .valign(Align::Center)
        .build()
}

fn conversation_list_row(
    message: &Message,
    count: usize,
    starred: bool,
    has_draft: bool,
) -> gtk::ListBoxRow {
    gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .child(&conversation_row_content(
            message, count, starred, has_draft,
        ))
        .build()
}

fn conversation_row_content(
    message: &Message,
    count: usize,
    starred: bool,
    has_draft: bool,
) -> gtk::Box {
    let sender = gtk::Label::builder()
        .label(if count > 1 {
            format!("{}  {count}", sender_name(message.header("From")))
        } else {
            sender_name(message.header("From"))
        })
        .halign(Align::Start)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .build();
    let date = gtk::Label::builder()
        .label(relative_message_date(message))
        .halign(Align::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let heading = gtk::Box::new(Orientation::Horizontal, 8);
    heading.append(&sender);
    if has_draft {
        heading.append(&draft_indicator());
    }
    if starred {
        heading.append(&gtk::Image::from_icon_name("starred-symbolic"));
    }
    heading.append(&date);

    let subject = gtk::Label::builder()
        .label(message.header("Subject"))
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
    content.set_margin_top(4);
    content.set_margin_bottom(4);
    content.set_margin_start(6);
    content.set_margin_end(6);
    content.append(&heading);
    content.append(&subject);
    content.append(&preview);
    content
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
        format!("Today at {}", date.format("%-I:%M %P"))
    } else if message_date == today - chrono::Duration::days(1) {
        format!("Yesterday at {}", date.format("%-I:%M %P"))
    } else if date.year() == today.year() {
        date.format("%-d %b at %-I:%M %P").to_string()
    } else {
        date.format("%-d %b %Y at %-I:%M %P").to_string()
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

fn attachment_control(
    widgets: &Widgets,
    email: &str,
    message: &Message,
    parts: &[&Payload],
    icon: &str,
    title: &str,
) -> Option<gtk::Widget> {
    if parts.is_empty() {
        return None;
    }
    let popover = gtk::Popover::new();
    let list = gtk::Box::new(Orientation::Vertical, 4);
    list.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["heading"])
            .build(),
    );
    for part in parts {
        let button = gtk::Button::new();
        let row = gtk::Box::new(Orientation::Horizontal, 6);
        row.append(&gtk::Image::from_icon_name(icon));
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
        list.append(&button);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(320)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    popover.set_child(Some(&scroll));
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    Some(
        gtk::MenuButton::builder()
            .child(&image)
            .always_show_arrow(false)
            .tooltip_text(format!("{title}: {}", attachment_summary(parts)))
            .popover(&popover)
            .valign(Align::Center)
            .css_classes(["flat", "postbird-part-trigger"])
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
            .title("Save file")
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
                MailClient::for_account(AccountStore::open()?, &email)?
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
    let container = &widgets.conversation_body;
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    let show_history = state.borrow().history_expanded;
    let visible = visible_conversation_indices(conversation.len(), show_history);
    for (index, message) in conversation.iter().rev().enumerate() {
        if index == 1 && conversation.len() > 4 {
            let hidden = conversation.len() - 3;
            let history_label = if show_history {
                format!("Hide {hidden} messages in between")
            } else {
                format!("Show {hidden} messages in between")
            };
            let history_button = gtk::Button::with_label(&history_label);
            history_button.add_css_class("flat");
            history_button.add_css_class("postbird-history-toggle");
            let widgets_for_history = widgets.clone();
            let state_for_history = state.clone();
            history_button.connect_clicked(move |_| {
                state_for_history.borrow_mut().history_expanded = !show_history;
                let load_images = widgets_for_history.preferences.borrow().load_remote_images;
                let messages = state_for_history.borrow().selected_conversation.clone();
                if !messages.is_empty() {
                    display_conversation(
                        &widgets_for_history,
                        &state_for_history,
                        &messages,
                        load_images,
                    );
                }
            });
            container.append(&history_button);
        }
        if !visible.contains(&index) {
            continue;
        }
        let is_latest = index + 1 == conversation.len();
        let heading = gtk::Label::builder()
            .label(sender_name(message.header("From")))
            .halign(Align::Start)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let date = gtk::Label::builder()
            .label(relative_message_date(message))
            .halign(Align::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let heading_line = gtk::Box::new(Orientation::Horizontal, 6);
        heading_line.append(&heading);
        let account_email = {
            state
                .borrow()
                .account_emails
                .get(widgets.account_picker.selected() as usize)
                .cloned()
        };
        if let Some(email) = account_email.as_deref() {
            let (attachments, inline_images) = message.attachment_groups();
            if let Some(control) = attachment_control(
                widgets,
                email,
                message,
                &inline_images,
                "image-x-generic-symbolic",
                "Embedded images",
            ) {
                heading_line.append(&control);
            }
            if let Some(control) = attachment_control(
                widgets,
                email,
                message,
                &attachments,
                "mail-attachment-symbolic",
                "Attachments",
            ) {
                heading_line.append(&control);
            }
        }
        heading_line.append(&date);
        let preview = gtk::Label::builder()
            .label(message_preview(&message.snippet))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let heading_row = gtk::Box::new(Orientation::Horizontal, 12);
        let header_details = gtk::Box::new(Orientation::Vertical, 6);
        header_details.set_hexpand(true);
        header_details.set_valign(Align::Center);
        header_details.append(&heading_line);
        header_details.append(&preview);
        let recipients = recipient_details(message);
        if let Some(recipients) = &recipients {
            recipients.set_visible(is_latest);
            header_details.append(recipients);
        }
        heading_row.append(&header_details);
        if message.label_ids.iter().any(|label| label == "DRAFT") {
            heading_row.append(&draft_indicator());
            let edit = gtk::Button::with_label("Edit Draft");
            edit.set_valign(Align::Center);
            let widgets = widgets.clone();
            let state = state.clone();
            let message = message.clone();
            edit.connect_clicked(move |button| edit_draft(&widgets, &state, &message, button));
            heading_row.append(&edit);
        }
        let body = gtk::Box::new(Orientation::Vertical, 6);
        body.set_hexpand(true);
        let expanded_content = gtk::Box::new(Orientation::Vertical, 6);
        expanded_content.append(&body);
        let expander = gtk::Expander::builder()
            .label_widget(&heading_row)
            .child(&expanded_content)
            .expanded(is_latest)
            .build();
        let preview_for_expander = preview.clone();
        let recipients_for_expander = recipients.clone();
        expander.connect_expanded_notify(move |expander| {
            preview_for_expander.set_visible(!expander.is_expanded());
            if let Some(recipients) = &recipients_for_expander {
                recipients.set_visible(expander.is_expanded());
            }
        });
        preview.set_visible(!is_latest);
        connect_message_body(
            &expander,
            &body,
            &widgets.conversation_scroll,
            message.clone(),
            account_email.clone(),
            load_remote_images,
        );
        expander.add_css_class("card");
        expander.add_css_class("message-section");
        container.append(&expander);
    }
}

fn visible_conversation_indices(count: usize, show_history: bool) -> Vec<usize> {
    if show_history || count <= 4 {
        (0..count).collect()
    } else {
        vec![0, count - 2, count - 1]
    }
}

// Keep collapsed messages entirely out of WebKit, including during initial display.
// Retain an expanded body when collapsed again to avoid repeated process launches.
fn connect_message_body(
    expander: &gtk::Expander,
    body: &gtk::Box,
    outer_scroll: &gtk::ScrolledWindow,
    message: Message,
    account_email: Option<String>,
    load_remote_images: bool,
) {
    let body = body.downgrade();
    let outer_scroll = outer_scroll.clone();
    let populate = move |expander: &gtk::Expander| {
        if expander.is_expanded()
            && let Some(body) = body.upgrade()
            && body.first_child().is_none()
        {
            populate_message_body(
                &body,
                &outer_scroll,
                &message,
                account_email.as_deref(),
                load_remote_images,
            );
        }
    };
    populate(expander);
    expander.connect_expanded_notify(populate);
}

fn needs_html_renderer(message: &Message) -> bool {
    message.label_ids.iter().any(|label| label == "DRAFT") || message.body_html().is_some()
}

fn append_text_body(body: &gtk::Box, text: &str) {
    let text = if text.trim().is_empty() {
        "This message has no text body."
    } else {
        text
    };
    let label = gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .yalign(0.0)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&label);
}

fn populate_message_body(
    body: &gtk::Box,
    outer_scroll: &gtk::ScrolledWindow,
    message: &Message,
    account_email: Option<&str>,
    load_remote_images: bool,
) {
    if !needs_html_renderer(message) {
        append_text_body(body, &message.body_text());
        return;
    }
    let view = new_message_webview();
    view.set_height_request(80);
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    let adjustment = outer_scroll.vadjustment();
    wheel.connect_scroll(move |controller, _, delta_y| {
        let maximum = (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
        let distance = match controller.unit() {
            gtk::gdk::ScrollUnit::Wheel => delta_y * 72.0,
            _ => delta_y,
        };
        adjustment.set_value((adjustment.value() + distance).clamp(adjustment.lower(), maximum));
        glib::Propagation::Stop
    });
    view.add_controller(wheel);
    let weak_body = body.downgrade();
    let text = message.body_text();
    let text_for_fit = text.clone();
    let text_for_tick = text.clone();
    view.connect_web_process_terminated(move |view, reason| {
        // This handles an established renderer failing. WebKit's fatal launch
        // handshake abort happens in the parent and cannot be caught here.
        eprintln!("message renderer terminated: {reason:?}; showing plain text");
        if let Some(body) = weak_body.upgrade()
            && view.parent().is_some()
        {
            body.remove(view);
            body.append(&gtk::Label::new(Some(
                "Message display stopped. Showing plain text.",
            )));
            append_text_body(&body, &text);
        }
    });
    let fitting = Rc::new(std::cell::Cell::new(false));
    let weak_body = body.downgrade();
    let fitting_after_load = fitting.clone();
    view.connect_load_changed(move |view, event| {
        if event == webkit6::LoadEvent::Finished
            && let Some(body) = weak_body.upgrade()
        {
            fit_webview_to_content(view, &body, &text_for_fit, &fitting_after_load);
        }
    });
    let weak_body = body.downgrade();
    let weak_view = view.downgrade();
    let last_width = Rc::new(std::cell::Cell::new(0));
    glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
        let Some(view) = weak_view.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if view.parent().is_none() {
            return glib::ControlFlow::Break;
        }
        let width = view.width();
        if width > 0
            && width != last_width.get()
            && !view.is_loading()
            && let Some(body) = weak_body.upgrade()
        {
            last_width.set(width);
            fit_webview_to_content(&view, &body, &text_for_tick, &fitting);
        }
        glib::ControlFlow::Continue
    });
    body.append(&view);
    let has_cid = message
        .body_html()
        .is_some_and(|html| html.to_ascii_lowercase().contains("cid:"));
    if !has_cid || message.inline_image_parts().is_empty() || account_email.is_none() {
        view.load_html(
            &message.rendered_body_with_images(&HashMap::new(), load_remote_images),
            None,
        );
        return;
    }
    let message = message.clone();
    let email = account_email.unwrap_or_default().to_owned();
    let weak_view = view.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let message_for_fetch = message.clone();
        let result =
            gio::spawn_blocking(move || embedded_image_uris(&message_for_fetch, &email)).await;
        let Some(view) = weak_view.upgrade().filter(|view| view.parent().is_some()) else {
            return;
        };
        let images = match result {
            Ok(Ok(images)) => images,
            Ok(Err(error)) => {
                eprintln!("could not load embedded images: {error:#}");
                HashMap::new()
            }
            Err(_) => {
                eprintln!("embedded image loading stopped unexpectedly");
                HashMap::new()
            }
        };
        view.load_html(
            &message.rendered_body_with_images(&images, load_remote_images),
            None,
        );
    });
}

fn embedded_image_uris(message: &Message, email: &str) -> anyhow::Result<HashMap<String, String>> {
    let mut images = HashMap::new();
    let mut deferred = Vec::new();
    for (part, content_id) in message.inline_image_parts() {
        let mime_type = part.mime_type.split(';').next().unwrap_or_default().trim();
        if !mime_type.starts_with("image/")
            || !mime_type.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.')
            })
        {
            continue;
        }
        if let Some(data) = &part.body.data {
            let bytes = crate::gmail::decode_attachment_data(data)?;
            images.insert(
                content_id.to_ascii_lowercase(),
                format!("data:{mime_type};base64,{}", STANDARD.encode(bytes)),
            );
        } else if let Some(path) = &part.body.attachment_id {
            deferred.push((
                content_id.to_ascii_lowercase(),
                mime_type.to_owned(),
                path.clone(),
            ));
        }
    }
    if !deferred.is_empty() {
        let paths = deferred
            .iter()
            .map(|(_, _, path)| path.clone())
            .collect::<Vec<_>>();
        let fetched = MailClient::for_account(AccountStore::open()?, email)?
            .inline_images(&message.id, &paths)?;
        for (id, mime_type, path) in deferred {
            if let Some(bytes) = fetched.get(&path) {
                images.insert(
                    id,
                    format!("data:{mime_type};base64,{}", STANDARD.encode(bytes)),
                );
            }
        }
    }
    Ok(images)
}

fn fit_webview_to_content(
    view: &webkit6::WebView,
    body: &gtk::Box,
    text: &str,
    fitting: &Rc<std::cell::Cell<bool>>,
) {
    if fitting.replace(true) {
        return;
    }
    let weak_view = view.downgrade();
    let body = body.downgrade();
    let text = text.to_owned();
    let fitting = fitting.clone();
    // Email script markup is disabled; this app-owned script only measures layout.
    view.evaluate_javascript(
        "Math.ceil(document.body ? Math.max(document.body.scrollHeight, document.body.offsetHeight) + 16 : document.documentElement.scrollHeight)",
        Some("postbird-sizing"),
        None,
        None::<&gio::Cancellable>,
        move |result| {
            fitting.set(false);
            let (Some(view), Some(body)) = (weak_view.upgrade(), body.upgrade()) else {
                return;
            };
            if view.parent().is_none() {
                return;
            }
            match result {
                Ok(height) if height.is_number() && height.to_double().is_finite()
                    && (20.0..=40_000.0).contains(&height.to_double()) => {
                    view.set_height_request((height.to_double().ceil() as i32).max(80));
                }
                _ => {
                    body.remove(&view);
                    body.append(&gtk::Label::new(Some("Showing plain text for this message.")));
                    append_text_body(&body, &text);
                }
            }
        },
    );
}

fn new_message_webview() -> webkit6::WebView {
    let settings = webkit6::Settings::new();
    settings.set_enable_javascript(true);
    settings.set_enable_javascript_markup(false);
    settings.set_enable_html5_database(false);
    settings.set_enable_html5_local_storage(false);
    // The document CSP controls remote image requests. WebKit's global switch
    // would also block the local data URLs used for embedded CID images.
    settings.set_auto_load_images(true);
    let view = webkit6::WebView::builder()
        .settings(&settings)
        .network_session(&webkit6::NetworkSession::new_ephemeral())
        .hexpand(true)
        .build();
    view.set_background_color(&gtk::gdk::RGBA::WHITE);
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
    if error.to_string() == "Request cancelled" {
        return;
    }
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
fn entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder().placeholder_text(placeholder).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a graphical session; tests the conversation viewport configuration"]
    fn conversation_viewport_does_not_jump_to_a_focused_webview() {
        gtk::init().expect("GTK display connection");
        let body = gtk::Box::new(Orientation::Vertical, 4);
        let scroll = new_conversation_scroll(&body);
        let viewport = scroll.child().unwrap().downcast::<gtk::Viewport>().unwrap();
        assert_eq!(viewport.child(), Some(body.upcast()));
        assert!(!viewport.is_scroll_to_focus());
    }

    #[test]
    fn goa_setup_failures_offer_actionable_paths() {
        let missing = GoaSetupIssue::from_backend_error(
            "GNOME Online Accounts Python bindings are unavailable",
        );
        let (heading, guidance) = missing.guidance();
        assert!(heading.contains("not ready"));
        assert!(guidance.contains("python-gobject"));
        assert!(guidance.contains("enable Mail"));
        assert!(guidance.contains("app password"));

        let (_, guidance) = GoaSetupIssue::NoMailAccount.guidance();
        assert!(guidance.contains("Open Online Accounts"));
        assert!(guidance.contains("Mail"));

        let (_, guidance) =
            GoaSetupIssue::from_backend_error("Could not connect to GNOME Online Accounts")
                .guidance();
        assert!(guidance.contains("Details: Could not connect"));
    }

    #[test]
    fn account_lookup_releases_state_before_selection_callback() {
        let state = Rc::new(RefCell::new(State {
            account_emails: vec![
                "first@example.com".to_owned(),
                "second@example.com".to_owned(),
                "third@example.com".to_owned(),
            ],
            ..State::default()
        }));

        assert_eq!(account_index(&state, "third@example.com"), Some(2));
        assert!(state.try_borrow_mut().is_ok());
    }

    #[test]
    fn long_conversations_keep_the_first_and_last_two_messages_visible() {
        assert_eq!(visible_conversation_indices(16, false), vec![0, 14, 15]);
        assert_eq!(visible_conversation_indices(4, false), vec![0, 1, 2, 3]);
        assert_eq!(visible_conversation_indices(5, true), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn account_tree_keeps_custom_folders_with_their_own_account() {
        let mut state = State::default();
        state.account_labels.insert(
            "one@example.com".to_owned(),
            vec![("Projects".to_owned(), "Projects".to_owned())],
        );
        let first = sidebar_folders(&state, "one@example.com");
        let second = sidebar_folders(&state, "two@example.com");
        assert!(first.iter().any(|(id, _, _)| id == "Projects"));
        assert!(!second.iter().any(|(id, _, _)| id == "Projects"));
        assert!(
            first
                .iter()
                .any(|(id, title, _)| id.is_empty() && title == "All Mail")
        );
        assert!(second.iter().any(|(id, _, _)| id == "INBOX"));
    }

    #[test]
    fn incremental_refresh_fetches_recent_and_missing_threads_only() {
        let references = ["recent", "cached", "new"]
            .into_iter()
            .map(|id| ThreadRef { id: id.into() })
            .collect::<Vec<_>>();
        let cached = vec![
            message("old-recent", "recent", 10),
            message("old-cached", "cached", 9),
            message("removed", "gone", 8),
        ];
        let needed = references_to_refresh(&references, &cached, 1);
        assert_eq!(
            needed
                .iter()
                .map(|reference| reference.id.as_str())
                .collect::<Vec<_>>(),
            vec!["recent", "new"]
        );
        let merged = merge_refreshed_threads(
            cached,
            &references,
            &needed,
            vec![
                message("fresh-recent", "recent", 11),
                message("fresh-new", "new", 12),
            ],
        );
        assert_eq!(
            merged
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh-new", "fresh-recent", "old-cached"]
        );
    }

    #[test]
    fn local_label_changes_are_idempotent_and_scoped_to_selected_messages() {
        let mut messages = vec![message("one", "thread", 1), message("two", "thread", 2)];
        let ids = HashSet::from(["one".to_owned()]);
        set_labels_for_messages(&mut messages, &ids, "STARRED", true);
        set_labels_for_messages(&mut messages, &ids, "STARRED", true);
        assert_eq!(messages[0].label_ids, vec!["STARRED"]);
        assert!(messages[1].label_ids.is_empty());
        set_labels_for_messages(&mut messages, &ids, "STARRED", false);
        assert!(messages[0].label_ids.is_empty());
    }

    #[test]
    fn optimistic_archives_hide_stale_results_but_not_new_mail_or_other_accounts() {
        let mut state = State::default();
        let key = ("me@example.com".to_owned(), "thread".to_owned());
        let conversation = vec![message("old", "thread", 10)];
        state.pending_archives.insert(key.clone());
        assert!(archive_is_hidden(&state, "me@example.com", &conversation));
        assert!(!archive_is_hidden(
            &state,
            "other@example.com",
            &conversation
        ));
        state.pending_archives.remove(&key);
        assert!(
            !archive_is_hidden(&state, "me@example.com", &conversation),
            "failed archives become visible again"
        );
        state.archived_messages.insert(
            key.clone(),
            (HashSet::from(["old".to_owned()]), std::time::Instant::now()),
        );
        assert!(archive_is_hidden(&state, "me@example.com", &conversation));
        let mut new_mail = conversation.clone();
        new_mail.push(message("new", "thread", 20));
        assert!(!archive_is_hidden(&state, "me@example.com", &new_mail));
        state.current_label = "SENT".to_owned();
        assert!(!archive_is_hidden(&state, "me@example.com", &conversation));
        state.current_label = "INBOX".to_owned();
        state.archived_messages.get_mut(&key).unwrap().1 =
            std::time::Instant::now() - std::time::Duration::from_secs(61);
        assert!(
            !archive_is_hidden(&state, "me@example.com", &conversation),
            "a later external move back to Inbox remains visible"
        );
    }

    #[test]
    fn draft_editor_preserves_headers_thread_and_attachments() {
        let message: Message = serde_json::from_value(json!({
            "id": "draft-message", "threadId": "conversation", "labelIds": ["DRAFT"],
            "payload": {"mimeType": "multipart/mixed", "headers": [
                {"name": "To", "value": "Alice <alice@example.com>"},
                {"name": "Cc", "value": "cc@example.com"},
                {"name": "Bcc", "value": "private@example.com"},
                {"name": "Subject", "value": "Existing subject"},
                {"name": "In-Reply-To", "value": "<original@example.com>"}
            ], "parts": [
                {"mimeType": "text/html", "body": {"data": "PGI-SGVsbG88L2I-"}},
                {"mimeType": "image/png", "headers": [{"name": "Content-ID", "value": "<image-1>"}], "body": {"data": "aW1hZ2U"}},
                {"mimeType": "text/plain", "filename": "notes.txt", "body": {"data": "bm90ZXM"}}
            ]}
        })).unwrap();
        let compose = draft_compose_message(&message).unwrap();
        assert_eq!(compose.to, "Alice <alice@example.com>");
        assert_eq!(compose.cc, "cc@example.com");
        assert_eq!(compose.bcc, "private@example.com");
        assert_eq!(compose.subject, "Existing subject");
        assert_eq!(compose.thread_id.as_deref(), Some("conversation"));
        assert_eq!(
            compose.in_reply_to.as_deref(),
            Some("<original@example.com>")
        );
        assert_eq!(compose.html_body.as_deref(), Some("<b>Hello</b>"));
        assert_eq!(compose.forwarded_attachments.len(), 2);
        assert_eq!(
            compose.forwarded_attachments[0].content_id.as_deref(),
            Some("image-1")
        );
        assert_eq!(&*compose.forwarded_attachments[1].data, b"notes");
    }

    #[test]
    #[ignore = "requires a graphical session; uses only synthetic messages"]
    fn message_body_lifecycle() {
        gtk::init().expect("GTK display connection");
        let editor = gtk::TextView::new();
        let _toolbar = rich_text_toolbar(&editor);
        editor.buffer().set_text("Hello");
        let original = rich_text_content(&editor);
        let html = "<p><b>Hello</b><img src='cid:image-1'></p>";
        assert_eq!(compose_body_content(&editor, &original, Some(html)).1, html);
        editor
            .buffer()
            .insert(&mut editor.buffer().end_iter(), " edited");
        let changed = compose_body_content(&editor, &original, Some(html));
        assert_eq!(changed.0, "Hello edited");
        assert!(changed.1.contains("Hello edited"));
        assert_ne!(changed.1, html);
        let mut message: Message = serde_json::from_value(serde_json::json!({
            "id": "synthetic-draft",
            "threadId": "synthetic-thread",
            "labelIds": [],
            "payload": {"mimeType": "text/plain", "body": {"data": "SGVsbG8"}}
        }))
        .unwrap();
        let body = gtk::Box::new(Orientation::Vertical, 6);
        let expander = gtk::Expander::builder().child(&body).build();
        let outer_scroll = gtk::ScrolledWindow::new();
        connect_message_body(
            &expander,
            &body,
            &outer_scroll,
            message.clone(),
            None,
            false,
        );
        assert!(
            body.first_child().is_none(),
            "collapsed messages stay unloaded"
        );
        expander.set_expanded(true);
        let first = body.first_child().unwrap();
        let label = first.clone().downcast::<gtk::Label>().unwrap();
        assert!(
            label.text().contains("Hello"),
            "plain-text message retains its text"
        );
        assert!(label.is_selectable());
        assert!(!label.uses_markup());
        expander.set_expanded(false);
        expander.set_expanded(true);
        assert_eq!(body.first_child().unwrap(), first, "reuse an existing body");
        let weak_body = body.downgrade();
        drop(expander);
        drop(body);
        assert!(
            weak_body.upgrade().is_none(),
            "callbacks must not retain removed bodies"
        );

        message.label_ids.push("DRAFT".to_owned());
        assert!(
            needs_html_renderer(&message),
            "plain-text drafts also use WebKit"
        );
        message.payload.mime_type = "text/html".to_owned();
        message.payload.body.data = Some("PGI-SGVsbG88L2I-".to_owned());
        let body = gtk::Box::new(Orientation::Vertical, 6);
        let expander = gtk::Expander::builder().child(&body).build();
        connect_message_body(&expander, &body, &outer_scroll, message, None, false);
        assert!(body.first_child().is_none());
        expander.set_expanded(true);
        let view = body
            .first_child()
            .unwrap()
            .downcast::<webkit6::WebView>()
            .unwrap();
        view.emit_by_name::<()>(
            "web-process-terminated",
            &[&webkit6::WebProcessTerminationReason::Crashed],
        );
        assert!(view.parent().is_none());
        assert!(body.last_child().unwrap().is::<gtk::Label>());
    }

    #[test]
    fn embedded_image_bytes_are_usable_without_remote_loading_or_an_account() {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id": "synthetic", "threadId": "thread", "payload": {
                "mimeType": "multipart/related", "parts": [
                    {"mimeType": "text/html", "body": {"data": "PGltZyBzcmM9J2NpZDpsb2dvJz4"}},
                    {"mimeType": "image/png", "headers": [{"name": "Content-ID", "value": "<logo>"}],
                     "body": {"data": "AQID"}}
                ]
            }
        }))
        .unwrap();
        let images = embedded_image_uris(&message, "unused@example.com").unwrap();
        assert_eq!(
            images.get("logo").map(String::as_str),
            Some("data:image/png;base64,AQID")
        );
        assert!(
            message
                .rendered_body_with_images(&images, false)
                .contains("src='data:image/png;base64,AQID'")
        );
    }
    use serde_json::json;

    #[test]
    fn forward_starts_a_new_message_with_original_context_and_no_recipients() {
        let original: Message = serde_json::from_value(json!({
            "id": "original", "threadId": "original-thread", "payload": {
                "mimeType": "text/plain", "body": {"data": "SGVsbG8"},
                "headers": [
                    {"name": "Subject", "value": "Report"},
                    {"name": "From", "value": "alice@example.com"},
                    {"name": "To", "value": "bob@example.com"},
                    {"name": "Cc", "value": "carol@example.com"},
                    {"name": "Bcc", "value": "private@example.com"}
                ]
            }
        }))
        .unwrap();
        let forward = forward_message(&original);
        assert_eq!(forward.subject, "Fwd: Report");
        assert!(forward.to.is_empty() && forward.cc.is_empty() && forward.bcc.is_empty());
        assert!(forward.thread_id.is_none() && forward.in_reply_to.is_none());
        assert!(forward.body.contains("From: alice@example.com"));
        assert!(forward.body.contains("Cc: carol@example.com"));
        assert!(forward.body.ends_with("Hello"));
        assert!(!forward.body.contains("private@example.com"));
        let mut forwarded_again = original;
        forwarded_again.payload.headers[0].value = forward.subject.clone();
        assert_eq!(forward_message(&forwarded_again).subject, forward.subject);
    }

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
        assert!(relative_message_date(&yesterday).starts_with("Yesterday at "));
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
