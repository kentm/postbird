use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use adw::prelude::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Datelike, Local};
use glib::variant::ToVariant;
use gtk::{Align, Orientation, gio, glib};
use lettre::message::{Mailbox, Mailboxes};
use webkit6::prelude::*;

use crate::{
    accounts::{Account, AccountKind, AccountStore},
    cache::MailCache,
    compose::{AttachmentList, InlineImages},
    gmail::{ComposeMessage, ForwardedAttachment, Message, Payload, ThreadRef},
    mail::{ImapClient, IncomingMail, MailClient, MailCursor},
    preferences::{FavoriteFolder, UiPreferences},
};

#[derive(Clone)]
struct Widgets {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    sidebar_toast: adw::ToastOverlay,
    activity: gtk::StringList,
    accounts: gtk::StringList,
    account_picker: gtk::DropDown,
    messages: gtk::ListBox,
    sidebar_navigation: gtk::Box,
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
    search: gtk::Entry,
    preferences: Rc<RefCell<UiPreferences>>,
}

struct State {
    account_emails: Vec<String>,
    account_names: Vec<String>,
    account_labels: HashMap<String, Vec<(String, String)>>,
    unread_counts: HashMap<(String, String), u32>,
    unread_refresh_generation: HashMap<String, u64>,
    favorite_refresh_inflight: HashSet<String>,
    favorite_refresh_pending: HashSet<String>,
    notification_cursors: HashMap<String, MailCursor>,
    notification_poll_inflight: HashSet<String>,
    notification_generation: u64,
    last_notification_sound: Option<std::time::Instant>,
    pending_notification_thread: Option<(String, String)>,
    mark_read_inflight: HashMap<String, usize>,
    collapsed_accounts: HashSet<String>,
    accounts_loaded: bool,
    conversations: Vec<Vec<Message>>,
    selected: Option<Message>,
    selected_conversation: Vec<Message>,
    history_expanded: bool,
    current_label: String,
    active_query: Option<String>,
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
            favorite_refresh_inflight: HashSet::new(),
            favorite_refresh_pending: HashSet::new(),
            notification_cursors: HashMap::new(),
            notification_poll_inflight: HashSet::new(),
            notification_generation: 0,
            last_notification_sound: None,
            pending_notification_thread: None,
            mark_read_inflight: HashMap::new(),
            collapsed_accounts: HashSet::new(),
            accounts_loaded: false,
            conversations: Vec::new(),
            selected: None,
            selected_conversation: Vec::new(),
            history_expanded: false,
            current_label: "INBOX".to_owned(),
            active_query: None,
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
    let sidebar_toast = adw::ToastOverlay::new();
    let state = Rc::new(RefCell::new(State::default()));
    let preferences = Rc::new(RefCell::new(UiPreferences::load()));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.set_top_bar_style(adw::ToolbarStyle::Flat);
    let header = adw::HeaderBar::new();
    header.add_css_class("postbird-header");
    header.add_css_class("flat");
    header.set_hexpand(true);
    let compose = gtk::Button::builder()
        .css_classes(["suggested-action"])
        .build();
    set_labeled_icon(&compose, "postbird-mail-plus-symbolic", "Compose");
    let settings = gtk::Button::builder()
        .icon_name("postbird-settings-symbolic")
        .tooltip_text("Settings")
        .css_classes(["flat", "postbird-settings-button"])
        .build();
    settings.update_property(&[gtk::accessible::Property::Label("Settings")]);
    header.pack_start(&compose);
    header.pack_end(&settings);
    let header_surface = gtk::Box::new(Orientation::Horizontal, 0);
    header_surface.add_css_class("postbird-header-surface");
    header_surface.append(&header);
    toolbar_view.add_top_bar(&header_surface);

    let accounts = gtk::StringList::new(&[]);
    let account_picker = gtk::DropDown::builder()
        .model(&accounts)
        .hexpand(true)
        .build();
    let messages = gtk::ListBox::new();
    messages.add_css_class("navigation-sidebar");
    messages.set_selection_mode(gtk::SelectionMode::Single);
    let sidebar_navigation = gtk::Box::new(Orientation::Vertical, 5);
    let search = mail_search_entry();
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

    let archive = action_button("postbird-archive-symbolic", "Archive");
    let star = action_button("postbird-star-check-symbolic", "Star");
    let trash = action_button("postbird-trash-symbolic", "Move to Trash");
    let unread = action_button("postbird-mail-check-symbolic", "Mark Unread");
    let reply = action_button("postbird-reply-symbolic", "Reply");
    let forward = action_button("postbird-forward-symbolic", "Forward");
    let reply_all = action_button("postbird-reply-all-symbolic", "Reply All");
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
    detail_actions.append(&organise_actions);
    detail_actions.append(&reply_actions);
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
        sidebar_toast,
        activity: gtk::StringList::new(&[]),
        accounts,
        account_picker,
        messages,
        sidebar_navigation,
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
    };
    let open_action = gio::SimpleAction::new("open-mail", Some(glib::VariantTy::STRING));
    let action_widgets = widgets.clone();
    let action_state = state.clone();
    open_action.connect_activate(move |_, parameter| {
        let Some((email, thread_id)) = parameter
            .and_then(|value| value.str())
            .and_then(|value| serde_json::from_str::<(String, String)>(value).ok())
        else {
            return;
        };
        if account_index(&action_state, &email).is_none() {
            return;
        }
        select_mailbox(&action_widgets, &action_state, &email, "INBOX");
        if !thread_id.is_empty() {
            action_state.borrow_mut().pending_notification_thread = Some((email, thread_id));
        }
        action_widgets.window.present();
    });
    app.add_action(&open_action);
    let state_for_shutdown = state.clone();
    app.connect_shutdown(move |app| {
        for email in &state_for_shutdown.borrow().account_emails {
            app.withdraw_notification(&new_mail_notification_id(email));
        }
    });

    let message_split = gtk::Paned::new(Orientation::Horizontal);
    message_split.add_css_class("postbird-message-split");
    message_split.set_start_child(Some(&message_list_panel(&widgets, &state)));
    message_split.set_end_child(Some(&detail_stack));
    message_split.set_position(preferences.borrow().message_split);
    message_split.set_wide_handle(false);
    message_split.set_resize_start_child(false);
    message_split.set_resize_end_child(true);
    message_split.set_shrink_start_child(false);
    message_split.set_shrink_end_child(false);
    message_split.set_hexpand(true);

    let mailbox_split = gtk::Paned::new(Orientation::Horizontal);
    mailbox_split.add_css_class("postbird-mailbox-split");
    mailbox_split.set_start_child(Some(&mailbox_sidebar(&widgets, &state)));
    mailbox_split.set_end_child(Some(&message_split));
    mailbox_split.set_position(preferences.borrow().mailbox_split.max(245));
    mailbox_split.set_wide_handle(false);
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
    connect_settings(&widgets, &state, &settings);
    connect_search(&widgets, &state);
    connect_message_actions(&widgets, &state);
    connect_reply(&widgets, &state);
    connect_forward(&widgets, &state);
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
        "window.postbird-window { font-size: 0.92em;
             --headerbar-bg-color: #202633; --headerbar-fg-color: #eff3fb; }
         .postbird-header-surface { background: #202633; }
         .postbird-header, .postbird-header:backdrop {
             background: transparent; color: #eff3fb; }
         .postbird-header .postbird-settings-button { color: #eff3fb; }
         .postbird-sidebar { background: #202633; color: #eff3fb; }
         .postbird-sidebar scrolledwindow, .postbird-sidebar viewport {
             background: #202633; }
         .postbird-sidebar label, .postbird-sidebar image { color: #eff3fb; }
         .postbird-sidebar .dim-label, .postbird-sidebar-heading { color: #aeb8c9; }
         .postbird-sidebar-heading { margin: 4px 4px 1px; letter-spacing: 0.08em; }
         .postbird-sidebar spinner { color: #aeb8c9; }
         .postbird-sidebar separator { background: #485062; margin: 5px 2px; }
         .postbird-sidebar expander { color: #eff3fb; }
         .postbird-account { margin-top: 2px; }
         .postbird-account-folders { margin: 2px 0 4px 8px; }
         .postbird-folder { min-height: 28px; padding: 2px 4px; border-radius: 7px; }
         .postbird-folder:hover { background: #39445a; }
         .postbird-folder.active { background: #315c9e; }
         .postbird-favorite-row.drop-before { box-shadow: inset 0 2px #7daaff; }
         .postbird-favorite-row.drop-after { box-shadow: inset 0 -2px #7daaff; }
         .postbird-favorite { min-width: 24px; padding: 0; opacity: 0.72; }
         .postbird-sidebar .postbird-unread-badge { background: #7daaff; color: #15243d;
             border-radius: 99px; padding: 1px 6px; font-weight: bold; font-size: 0.85em; }
         .postbird-mailbox-split { background: #202633; }
         .postbird-mailbox-split > separator { min-width: 3px; background: #394252;
             border: 0; box-shadow: none; background-image: none; }
         .postbird-message-split { background: #25282e; }
         .postbird-message-split > separator { min-width: 3px; background: #3a4353;
             border: 0; box-shadow: none; background-image: none; }
         .postbird-list-panel { --sidebar-bg-color: #25282e;
             --sidebar-fg-color: #edf0f5; }
         .postbird-list-panel, .postbird-list-panel scrolledwindow,
         .postbird-list-panel viewport, .postbird-list-panel list,
         .postbird-list-panel .navigation-sidebar {
             background: #25282e; color: #edf0f5; }
         .postbird-list-panel label, .postbird-list-panel image { color: #edf0f5; }
         .postbird-list-panel .dim-label { color: #afb4bd; }
         .postbird-list-panel row { background: transparent; }
         .postbird-list-panel row:hover { background: #343b49; }
         .postbird-list-panel row:selected { background: #185bb4; }
         .postbird-list-panel row:selected .dim-label { color: #dbe8ff; }
         .postbird-list-panel .postbird-unread-dot { min-width: 11px;
             color: #8db6ff; font-size: 11px; }
         .postbird-list-panel row.accent .postbird-row-subject { font-weight: 700; }
         .postbird-list-panel row:selected .postbird-unread-dot { color: #ffffff; }
         .postbird-reader, .postbird-reader scrolledwindow { background: #ffffff; color: #181b20; }
         .postbird-reader label, .postbird-reader image { color: #181b20; }
         .postbird-reader .dim-label { color: #717984; }
         .postbird-reader .postbird-message-title { font-size: 18px; font-weight: 700; }
         .postbird-reader .postbird-part-trigger { min-width: 20px; min-height: 0; padding: 0; }
         .postbird-reader .postbird-part-trigger > button { min-width: 0; min-height: 0;
             padding: 0; border: 0; box-shadow: none; background: transparent; }
         .postbird-reader .postbird-message-action { min-width: 20px; min-height: 0;
             padding: 0; border: 0; box-shadow: none; }
         .postbird-reader .message-section { background: #ffffff; color: #181b20;
             border: 0; border-bottom: 1px solid #e3e7ed; border-radius: 0; box-shadow: none; }
         .postbird-reader .postbird-history-toggle { background: #f0f4fa; border-radius: 12px;
             margin: 3px 0; min-height: 32px; }
         .postbird-reader .postbird-history-toggle label { color: #315c9e; }
         .postbird-action-group { background: #f2f4f7; border-radius: 12px; padding: 2px; }
         .postbird-action-group button { min-width: 34px; min-height: 30px; }
         .postbird-actions { margin-bottom: 11px; }
         .postbird-compose {
             --window-bg-color: #ffffff; --window-fg-color: #181b20;
             --view-bg-color: #ffffff; --view-fg-color: #181b20;
             --dialog-bg-color: #ffffff; --dialog-fg-color: #181b20;
             --headerbar-bg-color: #f2f4f7; --headerbar-fg-color: #181b20;
             --headerbar-backdrop-color: #f2f4f7;
             --card-bg-color: #ffffff; --card-fg-color: #181b20;
             --popover-bg-color: #ffffff; --popover-fg-color: #181b20;
             --border-color: #d9dde4;
             --accent-bg-color: #315c9e; --accent-fg-color: #ffffff;
             --accent-color: #315c9e;
             color: #181b20;
         }
         .postbird-compose textview, .postbird-compose textview text {
             background: #ffffff; color: #181b20; caret-color: #181b20;
         }
         .postbird-compose entry { background: #f2f4f7; color: #181b20; caret-color: #181b20; }
         .postbird-compose textview text selection, .postbird-compose entry selection {
             background: #315c9e; color: #ffffff;
         }
         .postbird-compose .dim-label { color: #626975; }",
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
        poll_new_mail_all(&widgets, &state);
        glib::ControlFlow::Continue
    });
}

fn poll_new_mail_all(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let preferences = widgets.preferences.borrow();
    if !preferences.notify_new_mail && !preferences.play_new_mail_sound {
        return;
    }
    drop(preferences);
    let emails = state.borrow().account_emails.clone();
    for email in emails {
        poll_new_mail_account(widgets, state, email);
    }
}

fn poll_new_mail_account(widgets: &Widgets, state: &Rc<RefCell<State>>, email: String) {
    let (cursor, generation) = {
        let mut state = state.borrow_mut();
        if !state.account_emails.contains(&email)
            || !state.notification_poll_inflight.insert(email.clone())
        {
            return;
        }
        (
            state.notification_cursors.get(&email).cloned(),
            state.notification_generation,
        )
    };
    let had_cursor = cursor.is_some();
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let request_email = email.clone();
        let result = gio::spawn_blocking(move || {
            MailClient::for_account(AccountStore::open()?, &request_email)?
                .poll_new_mail(cursor.as_ref())
        })
        .await;
        {
            let mut current = state.borrow_mut();
            current.notification_poll_inflight.remove(&email);
            if current.notification_generation != generation {
                drop(current);
                poll_new_mail_all(&widgets, &state);
                return;
            }
            if !current.account_emails.contains(&email) {
                return;
            }
        }
        match result {
            Ok(Ok(poll)) => {
                state
                    .borrow_mut()
                    .notification_cursors
                    .insert(email.clone(), poll.cursor);
                if had_cursor && !poll.messages.is_empty() {
                    alert_new_mail(&widgets, &state, &email, &poll.messages);
                }
            }
            Ok(Err(error)) => {
                record_activity(
                    &widgets,
                    &format!("Could not check new mail for {email}: {error}"),
                );
            }
            Err(_) => eprintln!("New mail check stopped unexpectedly for {email}"),
        }
    });
}

fn new_mail_notification_id(email: &str) -> String {
    format!("new-mail:{email}")
}

fn alert_new_mail(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    email: &str,
    messages: &[IncomingMail],
) {
    let preferences = widgets.preferences.borrow();
    let show_banner = preferences.notify_new_mail;
    let play_sound = preferences.play_new_mail_sound;
    drop(preferences);
    let distinct_count = messages
        .iter()
        .map(|message| message.id.as_str())
        .collect::<HashSet<_>>()
        .len();
    if show_banner && let Some(app) = widgets.window.application() {
        let notification = gio::Notification::new(&format!("New mail · {email}"));
        let body = if distinct_count == 1 {
            let message = &messages[0];
            let subject = if message.subject.is_empty() {
                "(No subject)"
            } else {
                &message.subject
            };
            format!("{} — {subject}", message.sender)
        } else {
            format!("{distinct_count} new messages")
        };
        notification.set_body(Some(&body.chars().take(180).collect::<String>()));
        notification.set_category(Some("email.arrived"));
        let thread_id = if distinct_count == 1 {
            messages[0].thread_id.as_str()
        } else {
            ""
        };
        if let Ok(target) = serde_json::to_string(&(email, thread_id)) {
            notification
                .set_default_action_and_target_value("app.open-mail", Some(&target.to_variant()));
        }
        app.send_notification(Some(&new_mail_notification_id(email)), &notification);
    }
    if play_sound {
        let mut state = state.borrow_mut();
        let can_play = state
            .last_notification_sound
            .is_none_or(|played| played.elapsed() >= std::time::Duration::from_secs(10));
        if can_play {
            state.last_notification_sound = Some(std::time::Instant::now());
            play_new_mail_sound();
        }
    }
}

fn play_new_mail_sound() {
    std::thread::spawn(|| {
        for event in ["message-new-email", "message-new-instant"] {
            let result = Command::new("canberra-gtk-play")
                .args(["-i", event, "-d", "New mail"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if result.is_ok_and(|status| status.success()) {
                return;
            }
        }
        eprintln!("Could not play the system new-mail sound");
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
    widgets.sidebar_toast.set_child(Some(&sidebar));
    widgets.sidebar_toast.clone().upcast()
}

fn sidebar_folders(state: &State, email: &str) -> Vec<(String, String, &'static str)> {
    let mut folders = [
        ("INBOX", "Inbox", "mail-unread-symbolic"),
        ("STARRED", "Starred", "postbird-star-check-symbolic"),
        ("SENT", "Sent", "postbird-send-symbolic"),
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
    button.set_tooltip_text(Some(&if account_name.is_some() {
        let unread = state
            .borrow()
            .unread_counts
            .get(&(email.to_owned(), label_id.to_owned()))
            .copied()
            .unwrap_or(0);
        if unread > 0 {
            format!("{title} — {email} · {unread} unread in this folder, including older mail. Use Unread to find them. Drag to reorder")
        } else {
            format!("{title} — {email} · Drag to reorder")
        }
    } else {
        format!("{title} — {email}")
    }));
    let widgets_for_select = widgets.clone();
    let state_for_select = state.clone();
    let selected_email = email.to_owned();
    let selected_label = label_id.to_owned();
    button.connect_clicked(move |_| {
        select_mailbox(
            &widgets_for_select,
            &state_for_select,
            &selected_email,
            &selected_label,
        );
    });
    row.append(&button);

    if account_name.is_some() {
        attach_favorite_reordering(widgets, state, &row, &button, email, label_id);
    }

    if account_name.is_some() {
        let rename = gtk::Button::builder()
            .icon_name("postbird-pencil-line-symbolic")
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
            "postbird-star-check-symbolic"
        } else {
            "postbird-star-symbolic"
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
        {
            let mut state = state_for_favorite.borrow_mut();
            state
                .unread_counts
                .remove(&(favorite_email.clone(), favorite_label.clone()));
            *state
                .unread_refresh_generation
                .entry(favorite_email.clone())
                .or_default() += 1;
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

fn available_favorite_folders(state: &State, preferences: &UiPreferences) -> Vec<FavoriteFolder> {
    state
        .account_emails
        .iter()
        .flat_map(|email| {
            sidebar_folders(state, email)
                .into_iter()
                .filter(|(label_id, _, _)| preferences.is_favorite(email, label_id))
                .map(|(label_id, _, _)| FavoriteFolder {
                    account_email: email.clone(),
                    label_id,
                })
        })
        .collect()
}

fn attach_favorite_reordering(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    row: &gtk::Box,
    button: &gtk::Button,
    email: &str,
    label_id: &str,
) {
    row.add_css_class("postbird-favorite-row");
    let folder = FavoriteFolder {
        account_email: email.to_owned(),
        label_id: label_id.to_owned(),
    };
    let source = gtk::DragSource::new();
    source.set_actions(gtk::gdk::DragAction::MOVE);
    let dragged_folder = folder.clone();
    source.connect_prepare(move |_, _, _| {
        let value = glib::BoxedAnyObject::new(dragged_folder.clone()).to_value();
        Some(gtk::gdk::ContentProvider::for_value(&value))
    });
    button.add_controller(source);

    let target = gtk::DropTarget::new(
        glib::BoxedAnyObject::static_type(),
        gtk::gdk::DragAction::MOVE,
    );
    let motion_row = row.clone();
    target.connect_motion(move |_, _, y| {
        motion_row.remove_css_class("drop-before");
        motion_row.remove_css_class("drop-after");
        motion_row.add_css_class(if y < f64::from(motion_row.height()) / 2.0 {
            "drop-before"
        } else {
            "drop-after"
        });
        gtk::gdk::DragAction::MOVE
    });
    let leave_row = row.clone();
    target.connect_leave(move |_| {
        leave_row.remove_css_class("drop-before");
        leave_row.remove_css_class("drop-after");
    });
    let drop_row = row.clone();
    let widgets_for_drop = widgets.clone();
    let state_for_drop = state.clone();
    target.connect_drop(move |_, value, _, y| {
        drop_row.remove_css_class("drop-before");
        drop_row.remove_css_class("drop-after");
        let Ok(payload) = value.get::<glib::BoxedAnyObject>() else {
            return false;
        };
        let Ok(source) = payload.try_borrow::<FavoriteFolder>() else {
            return false;
        };
        let source = source.clone();
        let available = {
            let state = state_for_drop.borrow();
            let preferences = widgets_for_drop.preferences.borrow();
            available_favorite_folders(&state, &preferences)
        };
        let moved = {
            let mut preferences = widgets_for_drop.preferences.borrow_mut();
            let previous = preferences.favorite_order.clone();
            if !preferences.move_favorite(
                &available,
                &source,
                &folder,
                y >= f64::from(drop_row.height()) / 2.0,
            ) {
                return false;
            }
            if let Err(error) = preferences.save() {
                preferences.favorite_order = previous;
                show_message(
                    &widgets_for_drop,
                    &format!("Could not save Favourites: {error}"),
                );
                return false;
            }
            true
        };
        if moved {
            let widgets = widgets_for_drop.clone();
            let state = state_for_drop.clone();
            glib::idle_add_local_once(move || rebuild_sidebar(&widgets, &state));
        }
        moved
    });
    row.add_controller(target);
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
    let favorites = {
        let state = state.borrow();
        let preferences = widgets.preferences.borrow();
        preferences.ordered_favorites(&available_favorite_folders(&state, &preferences))
    };
    for favorite in &favorites {
        if let Some((email, name, folders, _)) = accounts
            .iter()
            .find(|(email, _, _, _)| email == &favorite.account_email)
            && let Some((label_id, title, icon)) = folders
                .iter()
                .find(|(label_id, _, _)| label_id == &favorite.label_id)
        {
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
    if favorites.is_empty() {
        navigation.append(
            &gtk::Label::builder()
                .label("Star a folder to keep it here")
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    navigation.append(&gtk::Separator::new(Orientation::Horizontal));
    let accounts_heading = gtk::Box::new(Orientation::Horizontal, 0);
    let accounts_label = sidebar_heading("ACCOUNTS");
    accounts_label.set_hexpand(true);
    accounts_heading.append(&accounts_label);
    let spinner_slot = gtk::Box::new(Orientation::Horizontal, 0);
    spinner_slot.set_width_request(18);
    spinner_slot.set_height_request(18);
    spinner_slot.set_valign(Align::Center);
    spinner_slot.append(&widgets.mailbox_spinner);
    accounts_heading.append(&spinner_slot);
    navigation.append(&accounts_heading);
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
    let (generation, previous_counts) = {
        let mut state = state.borrow_mut();
        if state.mark_read_inflight.get(email).copied().unwrap_or(0) > 0 {
            state.favorite_refresh_pending.insert(email.to_owned());
            return;
        }
        if !state.favorite_refresh_inflight.insert(email.to_owned()) {
            state.favorite_refresh_pending.insert(email.to_owned());
            return;
        }
        state.favorite_refresh_pending.remove(email);
        let generation = state
            .unread_refresh_generation
            .entry(email.to_owned())
            .or_default();
        *generation += 1;
        let generation = *generation;
        let previous_counts = labels
            .iter()
            .map(|label| {
                (
                    label.clone(),
                    state
                        .unread_counts
                        .get(&(email.to_owned(), label.clone()))
                        .copied()
                        .unwrap_or(0),
                )
            })
            .collect::<HashMap<_, _>>();
        (generation, previous_counts)
    };
    let widgets = widgets.clone();
    let state = state.clone();
    let email = email.to_owned();
    glib::MainContext::default().spawn_local(async move {
        let request_email = email.clone();
        let request_labels = labels.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<_> {
            let counts = MailClient::for_account(AccountStore::open()?, &request_email)?
                .unread_counts(&request_labels)?;
            let mut prefetched = HashMap::new();
            let mut failed = HashSet::new();
            for (label, count) in request_labels.iter().zip(&counts) {
                if *count <= previous_counts.get(label).copied().unwrap_or(0) {
                    continue;
                }
                match prefetch_favorite_folder(&request_email, label, *count) {
                    Ok(messages) => {
                        prefetched.insert(label.clone(), messages);
                    }
                    Err(error) => {
                        eprintln!(
                            "Could not prefetch favourite {label} for {request_email}: {error}"
                        );
                        failed.insert(label.clone());
                    }
                }
            }
            Ok((counts, prefetched, failed))
        })
        .await;
        let mut visible_messages = None;
        let mut changed = false;
        let rerun = {
            let mut state = state.borrow_mut();
            state.favorite_refresh_inflight.remove(&email);
            let rerun = if state.mark_read_inflight.get(&email).copied().unwrap_or(0) == 0 {
                state.favorite_refresh_pending.remove(&email)
            } else {
                state.favorite_refresh_pending.insert(email.clone());
                false
            };
            if state.unread_refresh_generation.get(&email) == Some(&generation)
                && state.account_emails.contains(&email)
                && let Ok(Ok((counts, mut prefetched, failed))) = result
                && counts.len() == labels.len()
            {
                for (label, count) in labels.iter().zip(counts) {
                    if failed.contains(label) {
                        continue;
                    }
                    let key = (email.clone(), label.clone());
                    let old_count = state.unread_counts.get(&key).copied().unwrap_or(0);
                    if old_count == count {
                        continue;
                    }
                    changed = true;
                    if count > 0 {
                        state.unread_counts.insert(key, count);
                    } else {
                        state.unread_counts.remove(&key);
                    }
                    if state.current_label == *label
                        && !state.mailbox_loading
                        && widgets.search.text().is_empty()
                        && state
                            .account_emails
                            .get(widgets.account_picker.selected() as usize)
                            == Some(&email)
                    {
                        visible_messages = prefetched.remove(label);
                    }
                }
            }
            rerun
        };
        if changed {
            rebuild_sidebar(&widgets, &state);
        }
        if let Some(messages) = visible_messages {
            show_loaded_mailbox(&widgets, &state, messages, false, true);
        }
        if rerun {
            refresh_favorite_counts_for_account(&widgets, &state, &email);
        }
    });
}

fn select_mailbox(widgets: &Widgets, state: &Rc<RefCell<State>>, email: &str, label_id: &str) {
    let Some(index) = state
        .borrow()
        .account_emails
        .iter()
        .position(|item| item == email)
    else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        state.current_label = label_id.to_owned();
        state.active_query = None;
        state.pending_notification_thread = None;
    }
    widgets.search.set_text("");
    if widgets.account_picker.selected() != index as u32 {
        widgets.account_picker.set_selected(index as u32);
    } else {
        load_inbox(widgets, state, email.to_owned(), None, true);
    }
    rebuild_sidebar(widgets, state);
}

fn message_list_panel(widgets: &Widgets, state: &Rc<RefCell<State>>) -> gtk::Widget {
    let panel = gtk::Box::new(Orientation::Vertical, 5);
    panel.add_css_class("postbird-list-panel");
    panel.set_size_request(300, -1);
    let search_row = gtk::Box::new(Orientation::Horizontal, 5);
    search_row.set_margin_top(6);
    search_row.set_margin_start(6);
    search_row.set_margin_end(6);
    widgets.search.set_hexpand(true);
    search_row.append(&widgets.search);
    let unread_filter = gtk::Button::builder()
        .icon_name("postbird-mail-symbolic")
        .tooltip_text("Find unread mail in this folder, including older messages")
        .build();
    unread_filter.update_property(&[gtk::accessible::Property::Label("Unread mail")]);
    let widgets_for_unread = widgets.clone();
    let state_for_unread = state.clone();
    unread_filter.connect_clicked(move |_| {
        let index = widgets_for_unread.account_picker.selected() as usize;
        let Some(email) = state_for_unread.borrow().account_emails.get(index).cloned() else {
            return;
        };
        widgets_for_unread.search.set_text("is:unread");
        load_inbox(
            &widgets_for_unread,
            &state_for_unread,
            email,
            Some("is:unread".to_owned()),
            false,
        );
    });
    search_row.append(&unread_filter);
    let refresh = gtk::Button::builder()
        .icon_name("postbird-refresh-symbolic")
        .tooltip_text("Refresh this folder")
        .build();
    refresh.update_property(&[gtk::accessible::Property::Label("Refresh this folder")]);
    let widgets_for_refresh = widgets.clone();
    let state_for_refresh = state.clone();
    refresh.connect_clicked(move |_| {
        let index = widgets_for_refresh.account_picker.selected() as usize;
        let Some(email) = state_for_refresh
            .borrow()
            .account_emails
            .get(index)
            .cloned()
        else {
            return;
        };
        let query = widgets_for_refresh.search.text().trim().to_owned();
        let query = (!query.is_empty()).then_some(query);
        load_inbox(
            &widgets_for_refresh,
            &state_for_refresh,
            email.clone(),
            query,
            false,
        );
        refresh_favorite_counts_for_account(&widgets_for_refresh, &state_for_refresh, &email);
    });
    search_row.append(&refresh);
    panel.append(&search_row);
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
            let unread_messages = conversation
                .iter()
                .filter(|message| message.label_ids.iter().any(|label| label == "UNREAD"))
                .cloned()
                .collect::<Vec<_>>();
            if !unread_messages.is_empty() {
                let email = state
                    .borrow()
                    .account_emails
                    .get(widgets.account_picker.selected() as usize)
                    .cloned();
                for message in &mut conversation {
                    message.label_ids.retain(|label| label != "UNREAD");
                }
                let adjustments = if let Some(email) = email.as_deref() {
                    let mut state = state.borrow_mut();
                    let current_label = state.current_label.clone();
                    state.conversations[row_index] = conversation.clone();
                    *state
                        .mark_read_inflight
                        .entry(email.to_owned())
                        .or_default() += 1;
                    *state
                        .unread_refresh_generation
                        .entry(email.to_owned())
                        .or_default() += 1;
                    decrease_unread_counts(&mut state, email, &unread_messages, &current_label)
                } else {
                    state.borrow_mut().conversations[row_index] = conversation.clone();
                    HashMap::new()
                };
                if let Some(thread) = conversation.first() {
                    update_conversation_row(&widgets, &state, &thread.thread_id);
                }
                if let Some(email) = email {
                    if !adjustments.is_empty() {
                        rebuild_sidebar(&widgets, &state);
                    }
                    mark_read_in_background(
                        &widgets,
                        &state,
                        email,
                        unread_messages,
                        adjustments,
                        state.borrow().current_label.clone(),
                    );
                }
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

fn unread_count_deltas(messages: &[Message], current_label: &str) -> HashMap<String, u32> {
    let mut deltas = HashMap::new();
    for message in messages {
        let mut labels = message
            .label_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        // A conversation can contain Sent messages that are not in the opened
        // Inbox. Only add a known system folder when this message has its label.
        if !matches!(
            current_label,
            "INBOX" | "SENT" | "DRAFT" | "TRASH" | "STARRED"
        ) {
            labels.insert(current_label);
        }
        for label in labels {
            *deltas.entry(label.to_owned()).or_default() += 1;
        }
    }
    deltas
}

fn decrease_unread_counts(
    state: &mut State,
    email: &str,
    messages: &[Message],
    current_label: &str,
) -> HashMap<String, u32> {
    let mut applied = HashMap::new();
    for (label, count) in unread_count_deltas(messages, current_label) {
        let key = (email.to_owned(), label.clone());
        if let Some(old_count) = state.unread_counts.get(&key).copied() {
            let decrease = old_count.min(count);
            if decrease > 0 {
                let new_count = old_count - decrease;
                if new_count == 0 {
                    state.unread_counts.remove(&key);
                } else {
                    state.unread_counts.insert(key, new_count);
                }
                applied.insert(label, decrease);
            }
        }
    }
    applied
}

fn mark_read_in_background(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    email: String,
    unread_messages: Vec<Message>,
    count_adjustments: HashMap<String, u32>,
    current_label: String,
) {
    let message_ids = unread_messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let refresh_email = email.clone();
        let attempted_ids = message_ids.clone();
        let result =
            gio::spawn_blocking(move || -> anyhow::Result<(Vec<String>, Option<String>)> {
                let mut client = MailClient::for_account(AccountStore::open()?, &email)?;
                let result = client.set_unread_many(&message_ids, false)?;
                let marked_ids = result.updated;
                let failure = result.error;
                if !marked_ids.is_empty()
                    && let Err(error) =
                        MailCache::open().and_then(|mut cache| cache.mark_read(&email, &marked_ids))
                {
                    eprintln!("Could not update the read-state cache: {error}");
                }
                Ok((marked_ids, failure))
            })
            .await;
        let marked_ids = match result {
            Ok(Ok((marked_ids, failure))) => {
                restore_unmarked_messages(
                    &widgets,
                    &state,
                    &refresh_email,
                    &attempted_ids,
                    &marked_ids,
                );
                if let Some(error) = failure {
                    show_message(&widgets, &format!("Could not mark as read: {error}"));
                }
                marked_ids
            }
            Ok(Err(error)) => {
                restore_unmarked_messages(&widgets, &state, &refresh_email, &attempted_ids, &[]);
                show_message(&widgets, &format!("Could not mark as read: {error}"));
                Vec::new()
            }
            Err(_) => {
                restore_unmarked_messages(&widgets, &state, &refresh_email, &attempted_ids, &[]);
                show_message(&widgets, "The mark-as-read task stopped unexpectedly");
                Vec::new()
            }
        };
        let marked_ids = marked_ids.into_iter().collect::<HashSet<_>>();
        let failed_messages = unread_messages
            .iter()
            .filter(|message| !marked_ids.contains(&message.id))
            .cloned()
            .collect::<Vec<_>>();
        let failed_deltas = unread_count_deltas(&failed_messages, &current_label);
        let mut counts_changed = false;
        {
            let mut state = state.borrow_mut();
            if let Some(inflight) = state.mark_read_inflight.get_mut(&refresh_email) {
                *inflight -= 1;
                if *inflight == 0 {
                    state.mark_read_inflight.remove(&refresh_email);
                }
            }
            for (label, adjusted) in count_adjustments {
                let restore = adjusted.min(failed_deltas.get(&label).copied().unwrap_or(0));
                if restore > 0 {
                    *state
                        .unread_counts
                        .entry((refresh_email.clone(), label))
                        .or_default() += restore;
                    counts_changed = true;
                }
            }
        }
        if counts_changed {
            rebuild_sidebar(&widgets, &state);
        }
        refresh_favorite_counts_for_account(&widgets, &state, &refresh_email);
    });
}

fn restore_unmarked_messages(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    email: &str,
    attempted_ids: &[String],
    marked_ids: &[String],
) {
    let current_account = state
        .borrow()
        .account_emails
        .get(widgets.account_picker.selected() as usize)
        .cloned();
    if current_account.as_deref() != Some(email) {
        return;
    }
    let marked_ids = marked_ids.iter().collect::<HashSet<_>>();
    let failed_ids = attempted_ids
        .iter()
        .filter(|id| !marked_ids.contains(id))
        .cloned()
        .collect::<HashSet<_>>();
    if failed_ids.is_empty() {
        return;
    }
    update_visible_labels(state, &failed_ids, "UNREAD", true);
    let threads = state
        .borrow()
        .conversations
        .iter()
        .filter(|conversation| {
            conversation
                .iter()
                .any(|message| failed_ids.contains(&message.id))
        })
        .filter_map(|conversation| {
            conversation
                .first()
                .map(|message| message.thread_id.clone())
        })
        .collect::<Vec<_>>();
    for thread in threads {
        update_conversation_row(widgets, state, &thread);
    }
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
                state.borrow_mut().active_query = None;
                state.borrow_mut().pending_notification_thread = None;
                widgets.search.set_text("");
                load_inbox(&widgets, &state, email, None, true);
                rebuild_sidebar(&widgets, &state);
            }
        });
}

fn connect_settings(widgets: &Widgets, state: &Rc<RefCell<State>>, button: &gtk::Button) {
    let widgets = widgets.clone();
    let state = state.clone();
    button.connect_clicked(move |_| {
        let dialog = adw::PreferencesDialog::builder()
            .title("Settings")
            .search_enabled(false)
            .build();
        dialog.set_content_width(520);
        dialog.set_content_height(settings_height_for_accounts(0));
        let accounts_page = adw::PreferencesPage::builder()
            .title("Accounts")
            .icon_name("system-users-symbolic")
            .build();
        let accounts_group = adw::PreferencesGroup::builder()
            .title("Connected accounts")
            .build();
        let add_account = gtk::Button::builder()
            .label("Add account")
            .valign(Align::Center)
            .css_classes(["suggested-action"])
            .build();
        accounts_group.set_header_suffix(Some(&add_account));
        accounts_page.add(&accounts_group);
        dialog.add(&accounts_page);
        add_reading_settings(&dialog, &widgets, &state);
        add_notification_settings(&dialog, &widgets, &state);
        let activity_page = adw::PreferencesPage::builder()
            .title("Activity")
            .icon_name("document-open-recent-symbolic")
            .build();
        let activity_group = adw::PreferencesGroup::builder()
            .title("Recent messages")
            .description("The latest 100 notices from this session, newest first. Select text to copy it.")
            .build();
        let activity_text = gtk::Label::builder()
            .selectable(true).wrap(true).xalign(0.0).yalign(0.0)
            .margin_top(12).margin_bottom(12).margin_start(12).margin_end(12)
            .build();
        let update_activity = |label: &gtk::Label, activity: &gtk::StringList| {
            let text = (0..activity.n_items()).filter_map(|index| activity.string(index))
                .collect::<Vec<_>>().join("\n\n");
            label.set_text(if text.is_empty() { "No messages yet." } else { &text });
        };
        update_activity(&activity_text, &widgets.activity);
        let activity_weak = activity_text.downgrade();
        let activity_changed = widgets.activity.connect_items_changed(move |activity, _, _, _| {
            if let Some(label) = activity_weak.upgrade() {
                update_activity(&label, activity);
            }
        });
        let activity = widgets.activity.clone();
        let activity_changed = RefCell::new(Some(activity_changed));
        dialog.connect_closed(move |_| {
            if let Some(handler) = activity_changed.borrow_mut().take() {
                activity.disconnect(handler);
            }
        });
        activity_group.add(&gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(240).max_content_height(400)
            .propagate_natural_height(true).child(&activity_text).build());
        activity_page.add(&activity_group);
        dialog.add(&activity_page);
        let about_page = adw::PreferencesPage::builder()
            .title("About")
            .icon_name("help-about-symbolic")
            .build();
        let icons_group = adw::PreferencesGroup::builder()
            .title("Icons")
            .description("Button icons by Lucide Icons and Contributors. Used under the ISC license; the trash icon also includes Feather artwork under the MIT license.")
            .build();
        let icons_row = adw::ActionRow::builder().title("Lucide Icons").build();
        icons_row.add_suffix(&gtk::LinkButton::builder()
            .uri("https://lucide.dev/icons/")
            .label("View icons")
            .valign(Align::Center)
            .build());
        icons_group.add(&icons_row);
        about_page.add(&icons_group);
        dialog.add(&about_page);
        let rows = Rc::new(RefCell::new(Vec::<adw::ActionRow>::new()));

        let dialog_weak = dialog.downgrade();
        let group_weak = accounts_group.downgrade();
        let rows_for_add = rows.clone();
        let widgets_for_add = widgets.clone();
        let state_for_add = state.clone();
        add_account.connect_clicked(move |button| {
            let (Some(dialog), Some(group)) = (dialog_weak.upgrade(), group_weak.upgrade()) else {
                return;
            };
            let rows = rows_for_add.clone();
            button.set_sensitive(false);
            let button = button.clone();
            let widgets = widgets_for_add.clone();
            let state = state_for_add.clone();
            glib::MainContext::default().spawn_local(async move {
                add_account_from_settings(&widgets, &state, &dialog).await;
                refresh_settings_accounts(&widgets, &state, &dialog, &group, &rows);
                button.set_sensitive(true);
            });
        });

        dialog.present(Some(&widgets.window));
        refresh_settings_accounts(&widgets, &state, &dialog, &accounts_group, &rows);
    });
}

fn add_notification_settings(
    dialog: &adw::PreferencesDialog,
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
) {
    let page = adw::PreferencesPage::builder()
        .title("Notifications")
        .icon_name("preferences-system-notifications-symbolic")
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("New mail")
        .description("Alerts are checked only while Postbird is running.")
        .build();
    let preferences = widgets.preferences.borrow();
    let banner = adw::SwitchRow::builder()
        .title("Desktop notification")
        .subtitle("Show a banner with the sender and subject")
        .active(preferences.notify_new_mail)
        .build();
    let sound = adw::SwitchRow::builder()
        .title("System sound")
        .subtitle(
            if glib::find_program_in_path("canberra-gtk-play").is_some() {
                "Play the system mail sound, with or without a banner"
            } else {
                "Install canberra-gtk-play to use the system sound"
            },
        )
        .active(preferences.play_new_mail_sound)
        .build();
    sound.set_sensitive(glib::find_program_in_path("canberra-gtk-play").is_some());
    drop(preferences);
    group.add(&banner);
    group.add(&sound);
    page.add(&group);
    dialog.add(&page);

    let banner_widgets = widgets.clone();
    let banner_state = state.clone();
    let dialog_weak = dialog.downgrade();
    banner.connect_active_notify(move |row| {
        let mut preferences = banner_widgets.preferences.borrow_mut();
        let previously_enabled = preferences.notify_new_mail || preferences.play_new_mail_sound;
        preferences.notify_new_mail = row.is_active();
        let result = preferences.save();
        let now_enabled = preferences.notify_new_mail || preferences.play_new_mail_sound;
        drop(preferences);
        if let Err(error) = result
            && let Some(dialog) = dialog_weak.upgrade()
        {
            show_settings_error(&dialog, error);
        }
        if !row.is_active()
            && let Some(app) = banner_widgets.window.application()
        {
            for email in &banner_state.borrow().account_emails {
                app.withdraw_notification(&new_mail_notification_id(email));
            }
        }
        if !previously_enabled && now_enabled {
            let mut state = banner_state.borrow_mut();
            state.notification_cursors.clear();
            state.notification_generation += 1;
            drop(state);
            poll_new_mail_all(&banner_widgets, &banner_state);
        }
    });

    let sound_widgets = widgets.clone();
    let sound_state = state.clone();
    let dialog_weak = dialog.downgrade();
    sound.connect_active_notify(move |row| {
        let mut preferences = sound_widgets.preferences.borrow_mut();
        let previously_enabled = preferences.notify_new_mail || preferences.play_new_mail_sound;
        preferences.play_new_mail_sound = row.is_active();
        let result = preferences.save();
        let now_enabled = preferences.notify_new_mail || preferences.play_new_mail_sound;
        drop(preferences);
        if let Err(error) = result
            && let Some(dialog) = dialog_weak.upgrade()
        {
            show_settings_error(&dialog, error);
        }
        if !previously_enabled && now_enabled {
            let mut state = sound_state.borrow_mut();
            state.notification_cursors.clear();
            state.notification_generation += 1;
            drop(state);
            poll_new_mail_all(&sound_widgets, &sound_state);
        }
    });
}

async fn add_account_from_settings(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    settings: &adw::PreferencesDialog,
) {
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
    match choice.choose_future(Some(settings)).await.as_str() {
        "app-password" => add_app_password_account(widgets, state, settings).await,
        "goa" => add_goa_account(widgets, state, settings).await,
        _ => {}
    }
}

fn refresh_settings_accounts(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    settings: &adw::PreferencesDialog,
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
) {
    let accounts = match AccountStore::open().and_then(|store| store.accounts()) {
        Ok(accounts) => accounts,
        Err(error) => return show_settings_error(settings, error),
    };
    settings.set_content_height(settings_height_for_accounts(accounts.len()));
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    group.set_description(if accounts.is_empty() {
        Some("No accounts connected yet.")
    } else {
        None
    });
    for account in accounts {
        let method = match account.kind {
            AccountKind::GmailAppPassword => "Gmail app password",
            AccountKind::GnomeOnlineAccounts => "GNOME Online Accounts",
        };
        let row = adw::ActionRow::builder()
            .title(&account.email)
            .subtitle(method)
            .build();
        let remove = gtk::Button::builder()
            .label("Remove")
            .tooltip_text(format!("Remove {} from Postbird", account.email))
            .valign(Align::Center)
            .css_classes(["flat", "destructive-action"])
            .build();
        row.add_suffix(&remove);
        let email = account.email;
        let dialog_weak = settings.downgrade();
        let group_weak = group.downgrade();
        let rows_weak = Rc::downgrade(rows);
        let widgets = widgets.clone();
        let state = state.clone();
        remove.connect_clicked(move |button| {
            let (Some(dialog), Some(group), Some(rows)) = (
                dialog_weak.upgrade(),
                group_weak.upgrade(),
                rows_weak.upgrade(),
            ) else {
                return;
            };
            button.set_sensitive(false);
            let button = button.clone();
            let widgets = widgets.clone();
            let state = state.clone();
            let email = email.clone();
            glib::MainContext::default().spawn_local(async move {
                if remove_account_from_settings(&widgets, &state, &dialog, email).await {
                    refresh_settings_accounts(&widgets, &state, &dialog, &group, &rows);
                } else {
                    button.set_sensitive(true);
                }
            });
        });
        group.add(&row);
        rows.borrow_mut().push(row);
    }
}

fn settings_height_for_accounts(count: usize) -> i32 {
    // Keep the Notifications page comfortable even when no accounts are connected.
    // The Accounts page scrolls when its list is longer than the dialog.
    (200 + 56 * count.min(7) as i32).clamp(300, 560)
}

fn show_settings_message(settings: &adw::PreferencesDialog, message: &str) {
    settings.add_toast(adw::Toast::new(message));
}

fn show_settings_error(settings: &adw::PreferencesDialog, error: impl std::fmt::Display) {
    let dialog = adw::AlertDialog::builder()
        .heading("Postbird could not complete that action")
        .body(error.to_string())
        .build();
    dialog.add_response("close", "Close");
    dialog.present(Some(settings));
}

async fn add_app_password_account(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    settings: &adw::PreferencesDialog,
) {
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
    if dialog.choose_future(Some(settings)).await != "connect" {
        return;
    }
    let email = address.text().trim().to_owned();
    let secret = password.text().replace(' ', "");
    password.set_text("");
    if email.matches('@').count() != 1
        || email.chars().any(char::is_whitespace)
        || secret.is_empty()
    {
        return show_settings_message(settings, "Enter a full email address and app password");
    }
    show_settings_message(settings, "Checking Gmail IMAP and SMTP…");
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
            show_settings_message(settings, "Account connected");
        }
        Ok(Err(error)) => show_settings_error(settings, error),
        Err(_) => show_settings_message(settings, "The connection check stopped unexpectedly"),
    }
}

async fn add_goa_account(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    settings: &adw::PreferencesDialog,
) {
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
        match goa_setup_dialog(settings, &issue).await.as_str() {
            "retry" => continue,
            "app-password" => {
                add_app_password_account(widgets, state, settings).await;
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
    if dialog.choose_future(Some(settings)).await != "connect" {
        return;
    }
    let Some(selected) = accounts.get(picker.selected() as usize).cloned() else {
        return show_settings_message(settings, "Choose an Online Account");
    };
    show_settings_message(settings, "Checking GOA Gmail IMAP and SMTP…");
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
            show_settings_message(settings, "Account connected");
        }
        Ok(Err(error)) => show_settings_error(settings, error),
        Err(_) => show_settings_message(settings, "The Online Accounts check stopped unexpectedly"),
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

async fn goa_setup_dialog(
    settings_dialog: &adw::PreferencesDialog,
    issue: &GoaSetupIssue,
) -> String {
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
    let response = dialog.choose_future(Some(settings_dialog)).await;
    if response == "open-settings"
        && let Some((path, args)) = settings
    {
        match open_online_accounts_settings(&path, args) {
            Ok(()) => show_settings_message(
                settings_dialog,
                "Enable Mail in Online Accounts, then choose GOA in Postbird again",
            ),
            Err(error) => show_settings_error(
                settings_dialog,
                format!("Could not open Online Accounts: {error}"),
            ),
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

fn mail_search_entry() -> gtk::Entry {
    let search = gtk::Entry::builder()
        .placeholder_text("Search mail")
        .primary_icon_name("system-search-symbolic")
        .primary_icon_activatable(false)
        .secondary_icon_tooltip_text("Clear search")
        .accessible_role(gtk::AccessibleRole::SearchBox)
        .css_classes(["search"])
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Search mail")]);
    search.connect_changed(|search| {
        search.set_secondary_icon_name(
            (!search.text().is_empty()).then_some("postbird-circle-x-symbolic"),
        );
    });
    search.connect_icon_release(|search, position| {
        if position == gtk::EntryIconPosition::Secondary {
            search.set_text("");
            search.grab_focus();
        }
    });
    let keyboard = gtk::EventControllerKey::new();
    let search_weak = search.downgrade();
    keyboard.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape
            && let Some(search) = search_weak.upgrade()
            && !search.text().is_empty()
        {
            search.set_text("");
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    search.add_controller(keyboard);
    search
}

fn connect_search(widgets: &Widgets, state: &Rc<RefCell<State>>) {
    let widgets = widgets.clone();
    let state = state.clone();
    let widgets_for_clear = widgets.clone();
    let state_for_clear = state.clone();
    widgets.search.clone().connect_changed(move |search| {
        if !search.text().is_empty() {
            return;
        }
        // A text replacement briefly emits an empty value. Wait until it
        // finishes before deciding whether the folder needs reloading.
        let widgets = widgets_for_clear.clone();
        let state = state_for_clear.clone();
        glib::idle_add_local_once(move || {
            if !search_clear_needs_reload(
                state.borrow().active_query.as_deref(),
                widgets.search.text().as_str(),
            ) {
                return;
            }
            let index = widgets.account_picker.selected() as usize;
            let Some(email) = state.borrow().account_emails.get(index).cloned() else {
                return;
            };
            load_inbox(&widgets, &state, email, None, true);
        });
    });
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

fn search_clear_needs_reload(active_query: Option<&str>, text: &str) -> bool {
    active_query.is_some() && text.is_empty()
}

async fn remove_account_from_settings(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    settings: &adw::PreferencesDialog,
    email: String,
) -> bool {
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
    if dialog.choose_future(Some(settings)).await != "remove" {
        return false;
    }
    let removing_email = email.clone();
    let result =
        gio::spawn_blocking(move || AccountStore::open()?.remove_account(&removing_email)).await;
    match result {
        Ok(Ok(())) => {
            {
                let mut state = state.borrow_mut();
                state.load_generation += 1;
                state.load_cancel.store(true, Ordering::Relaxed);
                state
                    .pending_archives
                    .retain(|(account, _)| account != &email);
                state
                    .archived_messages
                    .retain(|(account, _), _| account != &email);
                *state
                    .unread_refresh_generation
                    .entry(email.clone())
                    .or_default() += 1;
                state.favorite_refresh_pending.remove(&email);
                state.current_label = "INBOX".to_owned();
            }
            reset_reader(widgets, state);
            widgets.search.set_text("");
            {
                let mut preferences = widgets.preferences.borrow_mut();
                preferences.remove_account(&email);
                if let Err(error) = preferences.save() {
                    show_settings_message(settings, &format!("Could not save Favourites: {error}"));
                }
            }
            load_accounts(widgets, state);
            if let Some(app) = widgets.window.application() {
                app.withdraw_notification(&new_mail_notification_id(&email));
            }
            show_settings_message(settings, "Account removed");
            true
        }
        Ok(Err(error)) => {
            show_settings_error(settings, error);
            false
        }
        Err(_) => {
            show_settings_message(settings, "The account removal task stopped unexpectedly");
            false
        }
    }
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
        let reply = reply_message(&original, &email, reply_all);
        present_compose(&widgets, email, Some(reply), Some(original), None);
    });
}

fn reply_message(original: &Message, email: &str, reply_all: bool) -> ComposeMessage {
    let subject = original.header("Subject");
    let (to, cc) = if reply_all {
        reply_all_recipients(original, email)
    } else {
        (reply_target(original), String::new())
    };
    ComposeMessage {
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
    }
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
        present_forward(&widgets, email, original, button);
    });
}

fn present_forward(widgets: &Widgets, email: String, original: Message, button: &gtk::Button) {
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
}

fn add_reading_settings(
    dialog: &adw::PreferencesDialog,
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
) {
    let page = adw::PreferencesPage::builder()
        .title("Reading")
        .icon_name("mail-read-symbolic")
        .build();
    let group = adw::PreferencesGroup::builder().title("Images").build();
    let remote_images = adw::SwitchRow::builder()
        .title("Remote Images")
        .subtitle("Allow emails to load images from the internet")
        .active(widgets.preferences.borrow().load_remote_images)
        .build();
    group.add(&remote_images);
    page.add(&group);
    dialog.add(&page);

    let widgets = widgets.clone();
    let state = state.clone();
    let dialog_weak = dialog.downgrade();
    remote_images.connect_active_notify(move |row| {
        let enabled = row.is_active();
        let mut preferences = widgets.preferences.borrow_mut();
        let previous = preferences.load_remote_images;
        if previous == enabled {
            return;
        }
        preferences.load_remote_images = enabled;
        if let Err(error) = preferences.save() {
            preferences.load_remote_images = previous;
            drop(preferences);
            row.set_active(previous);
            if let Some(dialog) = dialog_weak.upgrade() {
                show_settings_error(&dialog, error);
            }
            return;
        }
        drop(preferences);
        let conversation = state.borrow().selected_conversation.clone();
        if !conversation.is_empty() {
            display_conversation(&widgets, &state, &conversation, enabled);
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
    present_compose_with_title(widgets, account_email, initial, replying_to, editing, None);
}

fn present_compose_with_title(
    widgets: &Widgets,
    account_email: String,
    initial: Option<ComposeMessage>,
    replying_to: Option<Message>,
    editing: Option<EditingDraft>,
    title_override: Option<&str>,
) {
    let parent = widgets.window.clone();
    let title = compose_dialog_title(
        editing.is_some(),
        replying_to.is_some(),
        initial.is_some(),
        title_override,
    );
    let dialog = adw::Dialog::builder()
        .title(&title)
        .content_width(900)
        .content_height(720)
        .build();
    dialog.add_css_class("postbird-compose");
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let send = gtk::Button::builder()
        .css_classes(["suggested-action"])
        .build();
    set_labeled_icon(&send, "postbird-send-symbolic", "Send");
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
        .icon_name("postbird-paperclip-symbolic")
        .tooltip_text("Attach files")
        .build();
    set_labeled_icon(&attach, "postbird-paperclip-symbolic", "Attach files");
    header.pack_end(&send);
    header.pack_end(&save_draft);
    header.pack_start(&attach);
    toolbar.add_top_bar(&header);
    let form = gtk::Box::new(Orientation::Vertical, 8);
    form.set_margin_top(12);
    form.set_margin_bottom(12);
    form.set_margin_start(12);
    form.set_margin_end(12);
    let from_row = gtk::Box::new(Orientation::Horizontal, 8);
    let from_label = gtk::Label::new(Some("From"));
    from_label.set_margin_start(8);
    let from = gtk::Entry::builder()
        .text(&account_email)
        .editable(false)
        .hexpand(true)
        .tooltip_text("Sending account")
        .build();
    from.update_property(&[gtk::accessible::Property::Label("From")]);
    from_row.append(&from_label);
    from_row.append(&from);
    form.append(&from_row);
    let to = entry("To");
    let cc = entry("Cc");
    let bcc = entry("Bcc");
    let subject = entry("Subject");
    let body = crate::compose_history::editor();
    body.set_vexpand(true);
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    body.set_top_margin(12);
    body.set_bottom_margin(12);
    body.set_left_margin(12);
    body.set_right_margin(12);
    let formatting = rich_text_toolbar(&body);
    let inline_images = InlineImages::default();
    let restored_images = if let Some(initial) = &initial {
        to.set_text(&initial.to);
        cc.set_text(&initial.cc);
        bcc.set_text(&initial.bcc);
        subject.set_text(&initial.subject);
        inline_images.restore(&body, initial)
    } else {
        HashSet::new()
    };
    inline_images.connect_paste(&body, &dialog);
    let attachments = AttachmentList::new(initial.as_ref(), &restored_images);
    let parent_for_attachment = parent.downgrade();
    let dialog_for_attachment = dialog.downgrade();
    let files_for_attachment = attachments.clone();
    attach.connect_clicked(move |button| {
        let Some(parent) = parent_for_attachment.upgrade() else {
            return;
        };
        let files = files_for_attachment.clone();
        let dialog = dialog_for_attachment.clone();
        button.set_sensitive(false);
        let button = button.downgrade();
        glib::MainContext::default().spawn_local(async move {
            let picker = gtk::FileDialog::builder()
                .title("Attach files")
                .modal(true)
                .build();
            let result = picker.open_multiple_future(Some(&parent)).await;
            if let Some(button) = button.upgrade() {
                button.set_sensitive(true);
            }
            let Some(dialog) = dialog.upgrade().filter(|dialog| dialog.is_visible()) else {
                return;
            };
            match result {
                Ok(selected) => {
                    if let Err(error) = files.add_files(&selected) {
                        crate::compose::show_error(&dialog, error);
                    }
                }
                Err(error) if error.matches(gtk::DialogError::Dismissed) => {}
                Err(error) => crate::compose::show_error(&dialog, error),
            }
        });
    });
    body.add_css_class("card");
    for widget in [&to, &cc, &bcc, &subject] {
        form.append(widget);
    }
    form.append(&attachments.widget);
    form.append(&formatting);
    if initial
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

    let original_content = rich_text_content(&body, &inline_images);
    let original_html = initial
        .as_ref()
        .and_then(|message| message.html_body.clone());
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
    let attachments_send = attachments.clone();
    let images_for_send = inline_images.clone();
    let cancel_send = cancelled.clone();
    let reply_context_for_send = replying_to.clone();
    let title_for_send = title.clone();
    let send_started = Rc::new(Cell::new(false));
    send.connect_clicked(move |_| {
        if images_for_send.is_pending() {
            crate::compose::show_error(&dialog_for_send, "Wait for the pasted image to finish loading");
            return;
        }
        if send_started.replace(true) {
            return;
        }
        cancel_send.store(false, Ordering::Relaxed);
        let cancelled = cancel_send.clone();
        let editing = editing_for_send.clone();
        let (plain_body, html_body) =
            compose_body_content(&body_send, &content_for_send, html_for_send.as_deref(), &images_for_send);
        let (paths, mut files) = attachments_send.contents();
        files.extend(images_for_send.attachments(&body_send));
        let message = ComposeMessage {
            to: to_send.text().to_string(),
            cc: cc_send.text().to_string(),
            bcc: bcc_send.text().to_string(),
            subject: subject_send.text().to_string(),
            body: plain_body,
            html_body: Some(html_body),
            in_reply_to: reply_reference.clone(),
            thread_id: reply_thread.clone(),
            attachments: paths,
            forwarded_attachments: files,
        };
        let widgets = widgets_for_send.clone();
        let dialog = dialog_for_send.clone();
        let account_email = account_for_send.clone();
        let reply_context = reply_context_for_send.clone();
        let compose_title = title_for_send.clone();
        let sending_toast = adw::Toast::new("Sending…");
        sending_toast.set_timeout(0);
        sending_toast.set_priority(adw::ToastPriority::High);
        widgets.sidebar_toast.add_toast(sending_toast.clone());
        dialog.close();
        glib::MainContext::default().spawn_local(async move {
            let target = editing
                .as_ref()
                .map(|draft| (draft.id.clone(), draft.message_id.clone()));
            let send_message = message.clone();
            let send_account = account_email.clone();
            let result = gio::spawn_blocking(move || -> anyhow::Result<()> {
                let mut client = MailClient::for_account(AccountStore::open()?, &send_account)?;
                client.set_cancellation(Some(cancelled));
                if let Some(target) = target {
                    client.write_existing_draft(&target.0, &target.1, &send_message, true)?;
                } else {
                    client.send(&send_message)?;
                }
                Ok(())
            })
            .await;
            sending_toast.dismiss();
            match result {
                Ok(Ok(())) => {
                    show_sidebar_message(&widgets, "Message sent");
                    refresh_after_draft(&widgets, &editing);
                }
                Ok(Err(error)) => {
                    show_sidebar_message(&widgets, "Send failed");
                    present_compose_with_title(
                        &widgets,
                        account_email,
                        Some(message),
                        reply_context,
                        editing,
                        Some(&compose_title),
                    );
                    show_error(
                        &widgets,
                        format!("The message was reopened so you can review it. Delivery may be uncertain; check Sent before retrying.\n\n{error}"),
                    );
                }
                Err(_) => {
                    show_sidebar_message(&widgets, "Send failed");
                    present_compose_with_title(
                        &widgets,
                        account_email,
                        Some(message),
                        reply_context,
                        editing,
                        Some(&compose_title),
                    );
                    show_error(
                        &widgets,
                        "The send task stopped unexpectedly. The message was reopened; check Sent before retrying.",
                    );
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
    let attachments_draft = attachments;
    save_draft.connect_clicked(move |_| {
        if inline_images.is_pending() {
            crate::compose::show_error(
                &dialog_for_draft,
                "Wait for the pasted image to finish loading",
            );
            return;
        }
        cancelled.store(false, Ordering::Relaxed);
        let cancelled = cancelled.clone();
        set_busy(true);
        let editing = editing.clone();
        let (plain_body, html_body) = compose_body_content(
            &body,
            &original_content,
            original_html.as_deref(),
            &inline_images,
        );
        let (paths, mut files) = attachments_draft.contents();
        files.extend(inline_images.attachments(&body));
        let message = ComposeMessage {
            to: to.text().to_string(),
            cc: cc.text().to_string(),
            bcc: bcc.text().to_string(),
            subject: subject.text().to_string(),
            body: plain_body,
            html_body: Some(html_body),
            in_reply_to: reply_reference.clone(),
            thread_id: reply_thread.clone(),
            attachments: paths,
            forwarded_attachments: files,
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

fn compose_dialog_title(
    editing: bool,
    replying: bool,
    initial: bool,
    title_override: Option<&str>,
) -> String {
    title_override
        .unwrap_or(if editing {
            "Edit Draft"
        } else if replying {
            "Reply"
        } else if initial {
            "Forward"
        } else {
            "New message"
        })
        .to_owned()
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
            buffer.begin_user_action();
            buffer.remove_all_tags(&start, &end);
            buffer.end_user_action();
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
    buffer.begin_user_action();
    if start.has_tag(&tag) {
        buffer.remove_tag(&tag, &start, &end);
    } else {
        buffer.apply_tag(&tag, &start, &end);
    }
    buffer.end_user_action();
}

fn compose_body_content(
    editor: &gtk::TextView,
    original: &(String, String),
    original_html: Option<&str>,
    images: &InlineImages,
) -> (String, String) {
    let content = rich_text_content(editor, images);
    if &content == original
        && let Some(html) = original_html
    {
        (content.0, html.to_owned())
    } else {
        content
    }
}

fn rich_text_content(editor: &gtk::TextView, images: &InlineImages) -> (String, String) {
    let buffer = editor.buffer();
    let mut plain = String::new();
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
        if let Some(image) = images.at(&iter) {
            let cid = glib::markup_escape_text(image.content_id.as_deref().unwrap_or_default());
            let name = glib::markup_escape_text(&image.filename);
            let width = iter
                .paintable()
                .map_or(520, |paintable| paintable.intrinsic_width());
            html.push_str(&format!("<img src=\"cid:{cid}\" alt=\"{name}\" width=\"{width}\" style=\"max-width:100%;height:auto\">"));
            plain.push_str(&format!("[Image: {}]", image.filename));
            iter.forward_char();
            continue;
        }
        plain.push(iter.char());
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
    let previous_accounts = state.borrow().account_emails.clone();
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
        state.active_query = None;
    }
    widgets.search.set_text("");
    for account in accounts {
        widgets.accounts.append(&account.display_name);
        let mut state = state.borrow_mut();
        state.account_names.push(account.display_name);
        state.account_emails.push(account.email);
    }
    {
        let mut state = state.borrow_mut();
        initialize_account_collapse(&mut state, &widgets.preferences.borrow());
        let accounts = state.account_emails.iter().cloned().collect::<HashSet<_>>();
        if state.account_emails != previous_accounts {
            state.notification_generation += 1;
        }
        state
            .notification_cursors
            .retain(|email, _| accounts.contains(email));
        state
            .unread_counts
            .retain(|(email, _), _| accounts.contains(email));
    }
    let has_accounts = !state.borrow().account_emails.is_empty();
    widgets.account_picker.set_sensitive(has_accounts);
    if has_accounts {
        widgets.account_picker.set_selected(0);
        let email = state.borrow().account_emails[0].clone();
        load_inbox(widgets, state, email, None, false);
        for email in state.borrow().account_emails.clone() {
            load_labels(widgets, state, email);
        }
    } else {
        clear_list(&widgets.messages);
        add_status_row(
            &widgets.messages,
            "No account connected",
            "Open Settings to add a Gmail account.",
        );
    }
    rebuild_sidebar(widgets, state);
    refresh_favorite_counts(widgets, state);
    poll_new_mail_all(widgets, state);
}

fn initialize_account_collapse(state: &mut State, preferences: &UiPreferences) {
    let accounts = state.account_emails.iter().cloned().collect::<HashSet<_>>();
    if !state.accounts_loaded {
        let has_favorites = state.account_emails.iter().any(|email| {
            preferences.is_favorite(email, "INBOX")
                || preferences
                    .favorite_folders
                    .iter()
                    .any(|folder| folder.account_email == *email)
        });
        if has_favorites {
            state.collapsed_accounts = accounts;
        }
        state.accounts_loaded = true;
    } else {
        state
            .collapsed_accounts
            .retain(|email| accounts.contains(email));
    }
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
    let (email, cancelled, include_unread) = {
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
        if state.favorite_refresh_inflight.contains(&email) {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        state.inbox_sync_account = Some(email.clone());
        state.inbox_sync_cancel = Some(cancelled.clone());
        let include_unread = widgets.preferences.borrow().is_favorite(&email, "INBOX");
        (email, cancelled, include_unread)
    };
    update_mailbox_spinner(widgets, state);
    let widgets = widgets.clone();
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let sync_email = email.clone();
        let result = gio::spawn_blocking(move || -> anyhow::Result<Vec<Message>> {
            let mut client = MailClient::for_account(AccountStore::open()?, &sync_email)?;
            client.set_cancellation(Some(cancelled.clone()));
            let (references, _) = list_folder_references(&mut client, "INBOX", include_unread)?;
            let cached = MailCache::open()?.messages(&sync_email, "INBOX")?;
            let needed = references_to_refresh(&references, &cached, 3);
            let refreshed =
                fetch_threads_parallel(&sync_email, needed.clone(), Some(cancelled.clone()))?;
            let combined = merge_refreshed_threads(cached, &references, &needed, refreshed);
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

fn combine_thread_references(recent: Vec<ThreadRef>, unread: Vec<ThreadRef>) -> Vec<ThreadRef> {
    let mut seen = HashSet::new();
    recent
        .into_iter()
        .chain(unread)
        .filter(|reference| seen.insert(reference.id.clone()))
        .collect()
}

fn list_folder_references(
    client: &mut MailClient,
    label: &str,
    include_unread: bool,
) -> anyhow::Result<(Vec<ThreadRef>, Vec<ThreadRef>)> {
    let recent = client
        .list_threads_with_limit(Some(label), None, None, 50)?
        .threads;
    let unread = if include_unread {
        client
            .list_threads_with_limit(Some(label), Some("is:unread"), None, 50)?
            .threads
    } else {
        Vec::new()
    };
    let references = combine_thread_references(recent, unread.clone());
    Ok((references, unread))
}

fn references_to_prefetch(
    references: &[ThreadRef],
    unread: &[ThreadRef],
    cached: &[Message],
) -> Vec<ThreadRef> {
    let unread_threads = unread
        .iter()
        .map(|item| item.id.as_str())
        .collect::<HashSet<_>>();
    let cached_unread_threads = cached
        .iter()
        .filter(|message| message.label_ids.iter().any(|label| label == "UNREAD"))
        .map(|message| message.thread_id.as_str())
        .collect::<HashSet<_>>();
    references
        .iter()
        .enumerate()
        .filter(|(index, reference)| {
            *index < 10
                || unread_threads.contains(reference.id.as_str())
                || cached_unread_threads.contains(reference.id.as_str())
        })
        .map(|(_, reference)| reference.clone())
        .collect()
}

fn prefetch_favorite_folder(
    email: &str,
    label: &str,
    expected_unread: u32,
) -> anyhow::Result<Vec<Message>> {
    let mut client = MailClient::for_account(AccountStore::open()?, email)?;
    let (references, unread) = list_folder_references(&mut client, label, true)?;
    let cached = MailCache::open()?.messages(email, label)?;
    let needed = references_to_prefetch(&references, &unread, &cached);
    let refreshed = fetch_threads_parallel(email, needed.clone(), None)?;
    if expected_unread > 0
        && !refreshed
            .iter()
            .any(|message| message.label_ids.iter().any(|label| label == "UNREAD"))
    {
        anyhow::bail!("Gmail reported unread mail but returned no unread messages");
    }
    let messages = merge_refreshed_threads(cached, &references, &needed, refreshed);
    AccountStore::open()?.account(email)?;
    MailCache::open()?.replace_mailbox(email, label, &messages)?;
    Ok(messages)
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
        state.active_query = query.clone();
        (
            state.current_label.clone(),
            state.load_generation,
            state.load_cancel.clone(),
        )
    };
    update_mailbox_spinner(&widgets, &state);
    let include_unread =
        query.is_none() && widgets.preferences.borrow().is_favorite(&email, &label);
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
        let cache_email = email.clone();
        let mailbox_to_cache = cached_mailbox.clone();
        let first_page = gio::spawn_blocking(move || -> anyhow::Result<_> {
            let mut client = MailClient::for_account(AccountStore::open()?, &online_email)?;
            client.set_cancellation(Some(cancelled.clone()));
            let references = if let Some(query) = query.as_deref() {
                client
                    .list_threads(Some(&label), Some(query), None)?
                    .threads
            } else {
                list_folder_references(&mut client, &label, include_unread)?.0
            };
            let needed = references_to_refresh(&references, &cached_messages, 3);
            let (first, remaining) = split_refresh_batch(needed, 10);
            let refreshed =
                fetch_threads_parallel(&online_email, first.clone(), Some(cancelled.clone()))?;
            let messages = merge_refreshed_threads(cached_messages, &references, &first, refreshed);
            if let Some(mailbox) = mailbox_to_cache {
                if cancelled.load(Ordering::Relaxed) {
                    anyhow::bail!("Request cancelled");
                }
                AccountStore::open()?.account(&online_email)?;
                MailCache::open()?.replace_mailbox(&cache_email, &mailbox, &messages)?;
            }
            Ok((messages, references, remaining))
        })
        .await;
        if state.borrow().load_generation != generation {
            return;
        }
        let (messages, references, remaining) = match first_page {
            Ok(Ok(page)) => page,
            Ok(Err(error)) if showing_cached_messages => {
                state.borrow_mut().mailbox_loading = false;
                update_mailbox_spinner(&widgets, &state);
                show_message(
                    &widgets,
                    &format!("Showing cached mail; refresh failed: {error}"),
                );
                return;
            }
            Ok(Err(error)) => {
                state.borrow_mut().mailbox_loading = false;
                update_mailbox_spinner(&widgets, &state);
                clear_list(&widgets.messages);
                add_status_row(&widgets.messages, "Could not load mail", &error.to_string());
                show_error(&widgets, error);
                return;
            }
            Err(_) => {
                state.borrow_mut().mailbox_loading = false;
                update_mailbox_spinner(&widgets, &state);
                show_message(&widgets, "The inbox task stopped unexpectedly");
                return;
            }
        };
        show_loaded_mailbox(
            &widgets,
            &state,
            messages.clone(),
            select_first,
            showing_cached_messages,
        );
        if remaining.is_empty() {
            state.borrow_mut().mailbox_loading = false;
            update_mailbox_spinner(&widgets, &state);
            return;
        }

        let more_email = email.clone();
        let more_cache_email = email;
        let more_mailbox_to_cache = cached_mailbox;
        let more_cancelled = state.borrow().load_cancel.clone();
        let more = gio::spawn_blocking(move || -> anyhow::Result<Vec<Message>> {
            let refreshed = fetch_threads_parallel(
                &more_email,
                remaining.clone(),
                Some(more_cancelled.clone()),
            )?;
            let combined = merge_refreshed_threads(messages, &references, &remaining, refreshed);
            if let Some(mailbox) = more_mailbox_to_cache {
                if more_cancelled.load(Ordering::Relaxed) {
                    anyhow::bail!("Request cancelled");
                }
                AccountStore::open()?.account(&more_email)?;
                MailCache::open()?.replace_mailbox(&more_cache_email, &mailbox, &combined)?;
            }
            Ok(combined)
        })
        .await;
        if state.borrow().load_generation != generation {
            return;
        }
        state.borrow_mut().mailbox_loading = false;
        update_mailbox_spinner(&widgets, &state);
        match more {
            Ok(Ok(messages)) => show_loaded_mailbox(&widgets, &state, messages, select_first, true),
            Ok(Err(error)) => show_message(
                &widgets,
                &format!("More conversations could not be loaded: {error}"),
            ),
            Err(_) => show_message(&widgets, "The conversation load stopped unexpectedly"),
        }
    });
}

fn split_refresh_batch(
    references: Vec<ThreadRef>,
    first_count: usize,
) -> (Vec<ThreadRef>, Vec<ThreadRef>) {
    let mut remaining = references;
    let first = remaining
        .drain(..remaining.len().min(first_count))
        .collect();
    (first, remaining)
}

fn show_loaded_mailbox(
    widgets: &Widgets,
    state: &Rc<RefCell<State>>,
    messages: Vec<Message>,
    select_first: bool,
    showing_messages: bool,
) {
    if !should_display_loaded_messages(showing_messages, mailbox_changed(state, &messages)) {
        if select_pending_notification(widgets, state)
            || state.borrow().pending_notification_thread.is_some()
        {
            return;
        }
        if select_first && widgets.messages.selected_row().is_none() {
            select_first_conversation(widgets);
        }
        return;
    }
    let selected_thread = state
        .borrow()
        .selected
        .as_ref()
        .map(|message| message.thread_id.clone());
    display_messages(widgets, state, messages);
    if select_pending_notification(widgets, state)
        || state.borrow().pending_notification_thread.is_some()
    {
        return;
    }
    let selected_index = selected_thread.and_then(|thread_id| {
        state
            .borrow()
            .conversations
            .iter()
            .position(|conversation| {
                conversation
                    .first()
                    .is_some_and(|message| message.thread_id == thread_id)
            })
    });
    if let Some(row) = selected_index.and_then(|index| widgets.messages.row_at_index(index as i32))
    {
        widgets.messages.select_row(Some(&row));
    } else if select_first {
        select_first_conversation(widgets);
    } else {
        clear_reader_view(widgets, state);
    }
}

fn select_pending_notification(widgets: &Widgets, state: &Rc<RefCell<State>>) -> bool {
    let Some((email, thread_id)) = state.borrow().pending_notification_thread.clone() else {
        return false;
    };
    let index = widgets.account_picker.selected() as usize;
    if state.borrow().account_emails.get(index) != Some(&email) {
        return false;
    }
    let row_index = state
        .borrow()
        .conversations
        .iter()
        .position(|conversation| {
            conversation
                .first()
                .is_some_and(|message| message.thread_id == thread_id)
        });
    let Some(row) = row_index.and_then(|index| widgets.messages.row_at_index(index as i32)) else {
        return false;
    };
    state.borrow_mut().pending_notification_thread = None;
    widgets.messages.select_row(Some(&row));
    true
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
    let unread = conversation
        .iter()
        .any(|message| message.label_ids.contains(&"UNREAD".to_owned()));
    row.set_child(Some(&conversation_row_content(
        message,
        conversation.len(),
        starred,
        has_draft,
        unread,
    )));
    row.remove_css_class("accent");
    if unread {
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
        let unread = conversation
            .iter()
            .any(|message| message.label_ids.iter().any(|label| label == "UNREAD"));
        let row = conversation_list_row(message, conversation.len(), starred, has_draft, unread);
        if unread {
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
    unread: bool,
) -> gtk::ListBoxRow {
    gtk::ListBoxRow::builder()
        .activatable(true)
        .selectable(true)
        .child(&conversation_row_content(
            message, count, starred, has_draft, unread,
        ))
        .build()
}

fn conversation_row_content(
    message: &Message,
    count: usize,
    starred: bool,
    has_draft: bool,
    unread: bool,
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
    if unread {
        heading.append(
            &gtk::Label::builder()
                .label("●")
                .css_classes(["postbird-unread-dot"])
                .build(),
        );
    }
    heading.append(&sender);
    if has_draft {
        heading.append(&draft_indicator());
    }
    if starred {
        heading.append(&gtk::Image::from_icon_name("postbird-star-check-symbolic"));
    }
    heading.append(&date);

    let subject = gtk::Label::builder()
        .label(message.header("Subject"))
        .halign(Align::Start)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["postbird-row-subject"])
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

fn message_header_lines(message: &Message) -> Vec<String> {
    let mut lines = Vec::new();
    append_address_line(&mut lines, "From", message.header("From"));
    append_address_line(&mut lines, "Reply-To", message.header("Reply-To"));
    append_address_line(&mut lines, "To", message.header("To"));
    append_address_line(&mut lines, "Cc", message.header("Cc"));
    append_address_line(&mut lines, "Bcc", message.header("Bcc"));
    lines
}

fn message_header_details(message: &Message) -> Option<gtk::Label> {
    let lines = message_header_lines(message);
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
            .max_width_chars(64)
            .hexpand(true)
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
        .width_request(360)
        .propagate_natural_width(true)
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

fn message_reply_actions(widgets: &Widgets, email: &str, message: &Message) -> gtk::Box {
    let actions = gtk::Box::new(Orientation::Horizontal, 6);
    for (icon, title, reply_all) in [
        ("postbird-reply-symbolic", "Reply", Some(false)),
        ("postbird-reply-all-symbolic", "Reply All", Some(true)),
        ("postbird-forward-symbolic", "Forward", None),
    ] {
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(16);
        let button = gtk::Button::builder()
            .child(&image)
            .tooltip_text(title)
            .valign(Align::Center)
            .css_classes(["flat", "postbird-message-action"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(title)]);
        let widgets = widgets.clone();
        let email = email.to_owned();
        // Capture this message and its account, independently of the reader's
        // selected/latest message and any later account selection changes.
        let original = message.clone();
        button.connect_clicked(move |button| {
            if let Some(reply_all) = reply_all {
                let reply = reply_message(&original, &email, reply_all);
                present_compose(
                    &widgets,
                    email.clone(),
                    Some(reply),
                    Some(original.clone()),
                    None,
                );
            } else {
                present_forward(&widgets, email.clone(), original.clone(), button);
            }
        });
        actions.append(&button);
    }
    actions
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
            .tooltip_text(message.header("From"))
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
                "postbird-image-symbolic",
                "Embedded images",
            ) {
                heading_line.append(&control);
            }
            if let Some(control) = attachment_control(
                widgets,
                email,
                message,
                &attachments,
                "postbird-paperclip-symbolic",
                "Attachments",
            ) {
                heading_line.append(&control);
            }
            heading_line.append(&message_reply_actions(widgets, email, message));
        }
        heading_line.append(&date);
        let preview = gtk::Label::builder()
            .label(message_preview(&message.snippet))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let heading_row = gtk::Box::new(Orientation::Horizontal, 12);
        let header_details = gtk::Box::new(Orientation::Vertical, 2);
        header_details.set_hexpand(true);
        header_details.set_valign(Align::Center);
        header_details.append(&heading_line);
        header_details.append(&preview);
        let full_headers = message_header_details(message);
        if let Some(full_headers) = &full_headers {
            full_headers.set_visible(is_latest);
            header_details.append(full_headers);
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
        let headers_for_expander = full_headers.clone();
        expander.connect_expanded_notify(move |expander| {
            preview_for_expander.set_visible(!expander.is_expanded());
            if let Some(headers) = &headers_for_expander {
                headers.set_visible(expander.is_expanded());
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
    let document_committed = Rc::new(Cell::new(false));
    let committed_after_load = document_committed.clone();
    let weak_body = body.downgrade();
    let fitting_after_load = fitting.clone();
    view.connect_load_changed(move |view, event| {
        if event == webkit6::LoadEvent::Started {
            committed_after_load.set(false);
        } else if event == webkit6::LoadEvent::Committed {
            committed_after_load.set(true);
        }
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
            && document_committed.get()
            && (width != last_width.get() || view.is_loading())
            && let Some(body) = weak_body.upgrade()
        {
            last_width.set(width);
            fit_webview_to_content(&view, &body, &text_for_tick, &fitting);
        }
        glib::ControlFlow::Continue
    });
    body.append(&view);
    // Show the cached HTML immediately, including images already in the MIME
    // payload. Older cache entries may still need their embedded images fetched.
    let local_images = embedded_image_uris(message, None).unwrap_or_else(|error| {
        eprintln!("could not decode embedded images: {error:#}");
        HashMap::new()
    });
    view.load_html(
        &message.rendered_body_with_images(&local_images, load_remote_images),
        None,
    );
    let has_cid = message
        .body_html()
        .is_some_and(|html| html.to_ascii_lowercase().contains("cid:"));
    let needs_images = message
        .inline_image_parts()
        .iter()
        .any(|(part, _)| part.body.data.is_none() && part.body.attachment_id.is_some());
    if !has_cid || !needs_images || account_email.is_none() {
        return;
    }
    let message = message.clone();
    let email = account_email.unwrap_or_default().to_owned();
    let weak_view = view.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let message_for_fetch = message.clone();
        let result =
            gio::spawn_blocking(move || embedded_image_uris(&message_for_fetch, Some(&email)))
                .await;
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

fn embedded_image_uris(
    message: &Message,
    email: Option<&str>,
) -> anyhow::Result<HashMap<String, String>> {
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
    if !deferred.is_empty()
        && let Some(email) = email
    {
        let mut cache = MailCache::open()
            .map_err(|error| {
                eprintln!("could not open embedded image cache: {error:#}");
                error
            })
            .ok();
        let mut fetched = cache
            .as_ref()
            .and_then(|cache| {
                cache
                    .inline_images(email, &message.id)
                    .map_err(|error| {
                        eprintln!("could not read embedded image cache: {error:#}");
                        error
                    })
                    .ok()
            })
            .unwrap_or_default();
        let paths = deferred
            .iter()
            .map(|(_, _, path)| path.clone())
            .filter(|path| !fetched.contains_key(path))
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            let downloaded = MailClient::for_account(AccountStore::open()?, email)?
                .inline_images(&message.id, &paths)?;
            // An account may have been removed while the download was running.
            if AccountStore::open()?.account(email).is_ok()
                && let Some(cache) = cache.as_mut()
                && let Err(error) = cache.store_inline_images(email, &message.id, &downloaded)
            {
                eprintln!("could not cache embedded images: {error:#}");
            }
            fetched.extend(downloaded);
        }
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
        "document.readyState === 'loading' || !document.body ? null : Math.ceil(Math.max(document.body.scrollHeight, document.body.offsetHeight) + 16)",
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
                // Wait for the DOM, but not for remote images to finish loading.
                Ok(height) if height.is_null() => {}
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
    record_activity(widgets, &error.to_string());
    let dialog = adw::AlertDialog::builder()
        .heading("Postbird could not complete that action")
        .body(error.to_string())
        .build();
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.present(Some(&widgets.window));
}
fn show_message(widgets: &Widgets, message: &str) {
    record_activity(widgets, message);
    widgets.toast.add_toast(adw::Toast::new(message));
}
fn show_sidebar_message(widgets: &Widgets, message: &str) {
    record_activity(widgets, message);
    let toast = adw::Toast::new(message);
    toast.set_priority(adw::ToastPriority::High);
    widgets.sidebar_toast.add_toast(toast);
}
fn record_activity(widgets: &Widgets, message: &str) {
    let entry = format!("{} — {message}", Local::now().format("%H:%M:%S"));
    widgets.activity.splice(0, 0, &[entry.as_str()]);
    let count = widgets.activity.n_items();
    if count > 100 {
        widgets.activity.splice(100, count - 100, &[]);
    }
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
fn set_labeled_icon(button: &gtk::Button, icon: &str, label: &str) {
    let content = gtk::Box::new(Orientation::Horizontal, 6);
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(label)));
    button.set_child(Some(&content));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
}
fn entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder().placeholder_text(placeholder).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_height_grows_for_accounts_then_caps_for_long_lists() {
        assert_eq!(settings_height_for_accounts(0), 300);
        assert_eq!(settings_height_for_accounts(3), 368);
        assert_eq!(settings_height_for_accounts(20), 560);
    }

    #[test]
    fn accounts_start_collapsed_only_when_favorites_exist() {
        let mut state = State {
            account_emails: vec!["one@example.com".into(), "two@example.com".into()],
            ..State::default()
        };
        let mut preferences = UiPreferences::default();

        initialize_account_collapse(&mut state, &preferences);
        assert_eq!(state.collapsed_accounts.len(), 2);

        state.collapsed_accounts.remove("one@example.com");
        initialize_account_collapse(&mut state, &preferences);
        assert!(!state.collapsed_accounts.contains("one@example.com"));
        assert!(state.collapsed_accounts.contains("two@example.com"));

        let mut no_favorites = State {
            account_emails: state.account_emails.clone(),
            ..State::default()
        };
        preferences.toggle_favorite("one@example.com", "INBOX");
        preferences.toggle_favorite("two@example.com", "INBOX");
        initialize_account_collapse(&mut no_favorites, &preferences);
        assert!(no_favorites.collapsed_accounts.is_empty());

        let mut custom_favorite = State {
            account_emails: state.account_emails.clone(),
            ..State::default()
        };
        preferences.toggle_favorite("one@example.com", "Projects");
        initialize_account_collapse(&mut custom_favorite, &preferences);
        assert_eq!(custom_favorite.collapsed_accounts.len(), 2);
    }

    #[test]
    fn clearing_an_applied_search_restores_the_folder_view() {
        assert!(search_clear_needs_reload(Some("is:unread"), ""));
        assert!(search_clear_needs_reload(Some("from:friend"), ""));
        assert!(!search_clear_needs_reload(None, ""));
        assert!(!search_clear_needs_reload(Some("is:unread"), "new query"));
    }

    #[test]
    #[ignore = "requires a graphical session; tests the search clear control"]
    fn search_clear_icon_cancels_the_unread_filter() {
        gtk::init().expect("GTK display connection");
        let search = mail_search_entry();
        assert!(search.secondary_icon_name().is_none());
        search.set_text("is:unread");
        assert_eq!(
            search.secondary_icon_name().as_deref(),
            Some("postbird-circle-x-symbolic")
        );
        search.emit_by_name::<()>("icon-release", &[&gtk::EntryIconPosition::Secondary]);
        assert!(search.text().is_empty());
        assert!(search.secondary_icon_name().is_none());
        assert!(search_clear_needs_reload(
            Some("is:unread"),
            search.text().as_str()
        ));
    }

    #[test]
    fn first_mail_batch_is_small_and_leaves_the_rest_to_load() {
        let references = (0..25)
            .map(|index| ThreadRef {
                id: index.to_string(),
            })
            .collect();
        let (first, remaining) = split_refresh_batch(references, 10);
        assert_eq!(first.len(), 10);
        assert_eq!(first[0].id, "0");
        assert_eq!(remaining.len(), 15);
        assert_eq!(remaining[0].id, "10");
    }

    #[test]
    fn favorite_prefetch_includes_unread_threads_outside_the_recent_page() {
        let recent = (0..50)
            .map(|index| ThreadRef {
                id: format!("recent-{index}"),
            })
            .collect::<Vec<_>>();
        let unread = vec![ThreadRef {
            id: "old-unread".into(),
        }];
        let references = combine_thread_references(recent, unread.clone());
        assert_eq!(references.len(), 51);
        assert_eq!(references.last().unwrap().id, "old-unread");
        let needed = references_to_prefetch(&references, &unread, &[]);
        assert_eq!(needed.len(), 11);
        assert!(needed.iter().any(|reference| reference.id == "old-unread"));
        let old_unread_message = message("old-message", "old-unread", 1);
        let merged = merge_refreshed_threads(vec![old_unread_message], &references, &[], vec![]);
        assert_eq!(merged[0].thread_id, "old-unread");

        let mut previously_unread = message("cached", "recent-40", 42);
        previously_unread.label_ids.push("UNREAD".into());
        let needed = references_to_prefetch(&references, &[], &[previously_unread]);
        assert!(needed.iter().any(|reference| reference.id == "recent-40"));
    }

    #[test]
    fn opening_unread_conversation_updates_favorite_counts_immediately() {
        let mut first = message("one", "thread", 1);
        first.label_ids = vec!["UNREAD".into(), "INBOX".into()];
        let mut second = message("two", "thread", 2);
        second.label_ids = vec!["UNREAD".into(), "INBOX".into()];
        let mut state = State::default();
        state
            .unread_counts
            .insert(("reader@example.com".into(), "INBOX".into()), 3);
        state
            .unread_counts
            .insert(("reader@example.com".into(), "Projects".into()), 2);
        let applied = decrease_unread_counts(
            &mut state,
            "reader@example.com",
            &[first, second],
            "Projects",
        );
        assert_eq!(applied.get("INBOX"), Some(&2));
        assert_eq!(applied.get("Projects"), Some(&2));
        assert_eq!(
            state
                .unread_counts
                .get(&("reader@example.com".into(), "INBOX".into())),
            Some(&1)
        );
        assert!(
            !state
                .unread_counts
                .contains_key(&("reader@example.com".into(), "Projects".into()))
        );

        let mut sent = message("sent", "thread", 3);
        sent.label_ids = vec!["UNREAD".into(), "SENT".into()];
        assert!(!unread_count_deltas(&[sent], "INBOX").contains_key("INBOX"));
    }

    #[test]
    fn expanded_headers_reveal_full_sender_and_reply_to_addresses() {
        let message: Message = serde_json::from_value(json!({
            "id": "message",
            "threadId": "thread",
            "payload": {"headers": [
                {"name": "From", "value": "Trusted Name <unknown@example.org>"},
                {"name": "Reply-To", "value": "elsewhere@example.net"},
                {"name": "To", "value": "reader@example.com"}
            ]}
        }))
        .unwrap();
        assert_eq!(
            message_header_lines(&message),
            [
                "From: Trusted Name <unknown@example.org>",
                "Reply-To: elsewhere@example.net",
                "To: reader@example.com"
            ]
        );
    }

    #[test]
    fn reopened_send_keeps_its_original_compose_title() {
        assert_eq!(
            compose_dialog_title(false, false, false, None),
            "New message"
        );
        assert_eq!(compose_dialog_title(false, true, true, None), "Reply");
        assert_eq!(compose_dialog_title(false, false, true, None), "Forward");
        assert_eq!(compose_dialog_title(true, false, true, None), "Edit Draft");
        assert_eq!(
            compose_dialog_title(false, false, true, Some("New message")),
            "New message"
        );
    }

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
    fn cached_body_displays_while_remote_image_is_still_loading() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        gtk::init().expect("GTK display connection");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requested = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let server_requested = requested.clone();
        let server_release = release.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && !server_release.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let _ = stream.read(&mut [0; 2048]);
                    server_requested.store(true, Ordering::Relaxed);
                    while !server_release.load(Ordering::Relaxed) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let html = format!(
            "<div style='height:400px'>Cached message text</div><img src='http://{address}/slow.png'>"
        );
        let message: Message = serde_json::from_value(json!({
            "id": "slow-image-test", "payload": {"mimeType": "text/html",
                "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(html)}}
        }))
        .unwrap();
        let body = gtk::Box::new(Orientation::Vertical, 0);
        let scroll = new_conversation_scroll(&body);
        let window = gtk::Window::builder()
            .default_width(600)
            .default_height(600)
            .child(&scroll)
            .build();
        populate_message_body(&body, &scroll, &message, None, true);
        let view = body
            .first_child()
            .unwrap()
            .downcast::<webkit6::WebView>()
            .unwrap();
        window.present();
        let started = Instant::now();
        let context = glib::MainContext::default();
        let mut displayed = false;
        while started.elapsed() < Duration::from_secs(10) {
            while context.pending() {
                context.iteration(false);
            }
            if requested.load(Ordering::Relaxed)
                && view.is_loading()
                && view.height_request() >= 400
            {
                displayed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        release.store(true, Ordering::Relaxed);
        window.destroy();
        server.join().unwrap();
        assert!(
            displayed,
            "message body must be sized before the image server responds"
        );
    }

    #[test]
    #[ignore = "requires a graphical session; uses only synthetic messages"]
    fn compose_images_and_attachments_round_trip() {
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        assert!(
            gtk::gdk::Display::default()
                .unwrap()
                .type_()
                .name()
                .contains("Broadway"),
            "Run this clipboard test on an isolated Broadway display"
        );
        install_visual_style();
        let editor = crate::compose_history::editor();
        let _toolbar = rich_text_toolbar(&editor);
        let images = InlineImages::default();
        let dialog = adw::Dialog::new();
        let window = gtk::Window::builder()
            .default_width(800)
            .default_height(600)
            .child(&editor)
            .build();
        window.add_css_class("postbird-compose");
        window.present();
        let buffer = editor.buffer();
        buffer.set_text("Before REPLACE after");
        images.connect_paste(&editor, &dialog);
        buffer.select_range(&buffer.iter_at_offset(7), &buffer.iter_at_offset(14));
        let pixbuf =
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 1000, 600)
                .unwrap();
        pixbuf.fill(0x315c9eff);
        let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
        editor.clipboard().set_texture(&texture);
        editor.emit_by_name::<()>("paste-clipboard", &[]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while images.is_pending() && std::time::Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!images.is_pending());
        let (plain, html) = rich_text_content(&editor, &images);
        assert_eq!(plain, "Before [Image: Screenshot-1.png] after");
        assert!(html.contains("<img src=\"cid:postbird-"));
        let image = &images.attachments(&editor)[0];
        let saved_texture =
            gtk::gdk::Texture::from_bytes(&glib::Bytes::from(&*image.data)).unwrap();
        assert_eq!((saved_texture.width(), saved_texture.height()), (1000, 600));
        assert!(
            buffer
                .iter_at_offset(7)
                .paintable()
                .unwrap()
                .intrinsic_width()
                <= 520
        );

        buffer.begin_user_action();
        buffer.delete(&mut buffer.iter_at_offset(7), &mut buffer.iter_at_offset(8));
        buffer.end_user_action();
        assert!(
            images.attachments(&editor).is_empty(),
            "deleted images must not be sent"
        );
        editor.activate_action("text.undo", None).unwrap();
        assert_eq!(
            images.attachments(&editor).len(),
            1,
            "undo restores the inline image"
        );
        editor.activate_action("text.undo", None).unwrap();
        assert_eq!(
            rich_text_content(&editor, &images).0,
            "Before REPLACE after"
        );
        editor.activate_action("text.redo", None).unwrap();
        assert_eq!(
            images.attachments(&editor).len(),
            1,
            "redo restores a pasted image"
        );
        editor.activate_action("text.redo", None).unwrap();
        assert!(images.attachments(&editor).is_empty());
        editor.activate_action("text.undo", None).unwrap();
        assert_eq!(images.attachments(&editor).len(), 1);

        buffer.place_cursor(&buffer.end_iter());
        editor.clipboard().set_text(" pasted text");
        editor.emit_by_name::<()>("paste-clipboard", &[]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !rich_text_content(&editor, &images)
            .0
            .ends_with(" pasted text")
            && std::time::Instant::now() < deadline
        {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            rich_text_content(&editor, &images)
                .0
                .ends_with(" pasted text")
        );
        assert_eq!(images.attachments(&editor).len(), 1);
        buffer.select_range(&buffer.start_iter(), &buffer.iter_at_offset(6));
        toggle_selected_tag(&buffer, RICH_TAGS[0].0);
        assert!(
            rich_text_content(&editor, &images)
                .1
                .contains("<strong>Before</strong>")
        );
        editor.activate_action("text.undo", None).unwrap();
        assert!(!rich_text_content(&editor, &images).1.contains("<strong>"));
        editor.activate_action("text.undo", None).unwrap();
        assert_eq!(rich_text_content(&editor, &images).0, plain);
        assert_eq!(images.attachments(&editor).len(), 1);

        let message = ComposeMessage {
            to: "reader@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Screenshot".into(),
            body: plain.clone(),
            html_body: Some(html.clone()),
            in_reply_to: None,
            thread_id: None,
            attachments: Vec::new(),
            forwarded_attachments: images.attachments(&editor),
        };
        let reopened = gtk::TextView::new();
        let _toolbar = rich_text_toolbar(&reopened);
        let restored = InlineImages::default();
        let restored_ids = restored.restore(&reopened, &message);
        assert_eq!(restored_ids.len(), 1);
        assert_eq!(rich_text_content(&reopened, &restored).0, plain);
        assert_eq!(
            restored.attachments(&reopened)[0].data,
            message.forwarded_attachments[0].data
        );
        let original = rich_text_content(&reopened, &restored);
        assert_eq!(
            compose_body_content(&reopened, &original, Some(&html), &restored).1,
            html
        );

        let directory =
            std::env::temp_dir().join(format!("postbird-compose-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let first = directory.join("first.txt");
        let second = directory.join("second.txt");
        std::fs::write(&first, "first attachment").unwrap();
        std::fs::write(&second, "second attachment").unwrap();
        let files = gio::ListStore::new::<gio::File>();
        files.append(&gio::File::for_path(&first));
        files.append(&gio::File::for_path(&second));
        let attachments = AttachmentList::new(Some(&message), &restored_ids);
        attachments.add_files(files.upcast_ref()).unwrap();
        attachments.add_files(files.upcast_ref()).unwrap();
        assert_eq!(
            attachments.contents().0,
            vec![first.clone(), second.clone()]
        );
        assert!(
            attachments.contents().1.is_empty(),
            "inline images aren't listed again as file attachments"
        );
        let rows = attachments
            .widget
            .child()
            .unwrap()
            .downcast::<gtk::Viewport>()
            .unwrap()
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        rows.first_child()
            .unwrap()
            .last_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap()
            .emit_clicked();
        assert_eq!(attachments.contents().0, vec![second]);
        assert!(
            editor.color().red() < 0.2,
            "the editor stays dark text on its light surface"
        );
        window.destroy();

        // Exercise the real compose dialog under a forced dark application theme.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let window = adw::ApplicationWindow::builder()
            .default_width(1050)
            .default_height(820)
            .build();
        let widgets = Widgets {
            window: window.clone(),
            toast: adw::ToastOverlay::new(),
            sidebar_toast: adw::ToastOverlay::new(),
            activity: gtk::StringList::new(&[]),
            accounts: gtk::StringList::new(&[]),
            account_picker: gtk::DropDown::from_strings(&[]),
            messages: gtk::ListBox::new(),
            sidebar_navigation: gtk::Box::new(Orientation::Vertical, 0),
            mailbox_spinner: gtk::Spinner::new(),
            message_title: gtk::Label::new(None),
            message_sender: gtk::Label::new(None),
            conversation_scroll: gtk::ScrolledWindow::new(),
            conversation_body: gtk::Box::new(Orientation::Vertical, 0),
            message_content: gtk::Box::new(Orientation::Vertical, 0),
            detail_stack: gtk::Stack::new(),
            archive: gtk::Button::new(),
            star: gtk::Button::new(),
            trash: gtk::Button::new(),
            unread: gtk::Button::new(),
            reply: gtk::Button::new(),
            reply_all: gtk::Button::new(),
            forward: gtk::Button::new(),
            search: mail_search_entry(),
            preferences: Rc::new(RefCell::new(UiPreferences::default())),
        };
        let mut preview_message = message;
        preview_message.attachments = attachments.contents().0;
        window.present();
        present_compose_with_title(
            &widgets,
            "sender@example.com".into(),
            Some(preview_message),
            None,
            None,
            Some("New message"),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            while glib::MainContext::default().pending() {
                glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let dialog = window.visible_dialog().unwrap();
        // AdwDialog covers the whole parent window. Its background must stay
        // transparent; --dialog-bg-color paints only the actual compose sheet.
        #[allow(deprecated)]
        let background = {
            let snapshot = gtk::Snapshot::new();
            snapshot.render_background(&dialog.style_context(), 0.0, 0.0, 100.0, 100.0);
            snapshot.to_node()
        };
        assert!(
            background.is_none(),
            "compose must not paint over the mailbox"
        );
        fn check_editor_colors(widget: &gtk::Widget) -> usize {
            let mut checked = 0;
            if widget.is::<gtk::TextView>() || widget.is::<gtk::Entry>() {
                assert!(
                    widget.color().red() < 0.2,
                    "compose fields use dark text even in dark mode"
                );
                checked += 1;
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                checked += check_editor_colors(&widget);
                child = widget.next_sibling();
            }
            checked
        }
        assert_eq!(check_editor_colors(&dialog.child().unwrap()), 6);
        dialog.close();
        check_message_reply_actions(&widgets);
        window.destroy();
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn check_message_reply_actions(widgets: &Widgets) {
        fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
            let mut result = vec![widget.clone()];
            let mut child = widget.first_child();
            while let Some(widget) = child {
                result.extend(descendants(&widget));
                child = widget.next_sibling();
            }
            result
        }
        fn wait_until(condition: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !condition() && std::time::Instant::now() < deadline {
                glib::MainContext::default().iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(condition(), "compose dialog did not finish opening/closing");
        }
        wait_until(|| widgets.window.visible_dialog().is_none());
        let older: Message = serde_json::from_value(json!({
            "id": "older", "threadId": "conversation", "payload": {
                "mimeType": "multipart/mixed", "headers": [
                    {"name": "Subject", "value": "Earlier subject"},
                    {"name": "From", "value": "alice@example.com"},
                    {"name": "Reply-To", "value": "team@example.com"},
                    {"name": "To", "value": "sender@example.com, bob@example.com"},
                    {"name": "Cc", "value": "carol@example.com"},
                    {"name": "Message-ID", "value": "<older@example.com>"}
                ], "parts": [
                    {"mimeType": "text/plain", "body": {"data": "T2xkIGJvZHk"}},
                    {"mimeType": "text/plain", "filename": "older-only.txt", "body": {"data": "b2xkIGZpbGU"}}
                ]
            }
        })).unwrap();
        let newer: Message = serde_json::from_value(json!({
            "id": "newer", "threadId": "conversation", "payload": {
                "mimeType": "text/plain", "body": {"data": "TmV3IGJvZHk"},
                "headers": [
                    {"name": "Subject", "value": "Latest subject"},
                    {"name": "From", "value": "different@example.com"},
                    {"name": "Message-ID", "value": "<newer@example.com>"}
                ]
            }
        }))
        .unwrap();
        let state = Rc::new(RefCell::new(State {
            account_emails: vec!["sender@example.com".into(), "other@example.com".into()],
            selected: Some(newer.clone()),
            ..State::default()
        }));
        widgets
            .account_picker
            .set_model(Some(&gtk::StringList::new(&["Sender", "Other"])));
        widgets.account_picker.set_selected(0);
        display_conversation(widgets, &state, &[newer, older], false);
        let older_row = widgets.conversation_body.first_child().unwrap();
        let buttons = descendants(&older_row)
            .into_iter()
            .filter(|widget| widget.has_css_class("postbird-message-action"))
            .map(|widget| widget.downcast::<gtk::Button>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(buttons.len(), 3);
        // Changing the reader's account cannot change a message action's sender.
        widgets.account_picker.set_selected(1);
        for (index, button) in buttons.iter().enumerate() {
            assert_eq!(
                button
                    .child()
                    .unwrap()
                    .downcast::<gtk::Image>()
                    .unwrap()
                    .pixel_size(),
                16
            );
            button.emit_clicked();
            wait_until(|| widgets.window.visible_dialog().is_some());
            let dialog = widgets.window.visible_dialog().unwrap();
            let content = descendants(&dialog.child().unwrap());
            let entries = content
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Entry>())
                .collect::<Vec<_>>();
            let field = |placeholder: &str| {
                entries
                    .iter()
                    .find(|entry| entry.placeholder_text().as_deref() == Some(placeholder))
                    .unwrap()
                    .text()
                    .to_string()
            };
            assert_eq!(
                entries
                    .iter()
                    .find(|entry| !entry.is_editable())
                    .unwrap()
                    .text(),
                "sender@example.com"
            );
            assert_eq!(
                field("Subject"),
                if index == 2 {
                    "Fwd: Earlier subject"
                } else {
                    "Re: Earlier subject"
                }
            );
            assert_eq!(
                field("To"),
                match index {
                    0 => "team@example.com",
                    1 => "team@example.com, bob@example.com",
                    _ => "",
                }
            );
            assert_eq!(
                field("Cc"),
                if index == 1 { "carol@example.com" } else { "" }
            );
            let bodies = content
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::TextView>())
                .map(|view| {
                    let buffer = view.buffer();
                    buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), true)
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(bodies.contains("Old body"));
            assert!(!bodies.contains("New body"));
            if index == 2 {
                assert!(
                    content
                        .iter()
                        .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                        .any(|label| label.text() == "older-only.txt")
                );
            }
            dialog.close();
            wait_until(|| widgets.window.visible_dialog().is_none());
        }
    }

    #[test]
    #[ignore = "requires a graphical session; uses only synthetic messages"]
    fn message_body_lifecycle() {
        gtk::init().expect("GTK display connection");
        let editor = gtk::TextView::new();
        let _toolbar = rich_text_toolbar(&editor);
        editor.buffer().set_text("Hello");
        let images = InlineImages::default();
        let original = rich_text_content(&editor, &images);
        let html = "<p><b>Hello</b><img src='cid:image-1'></p>";
        assert_eq!(
            compose_body_content(&editor, &original, Some(html), &images).1,
            html
        );
        editor
            .buffer()
            .insert(&mut editor.buffer().end_iter(), " edited");
        let changed = compose_body_content(&editor, &original, Some(html), &images);
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
        let images = embedded_image_uris(&message, None).unwrap();
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

    #[test]
    fn reply_reference_belongs_to_the_requested_message() {
        let original: Message = serde_json::from_value(json!({
            "id": "older-message", "threadId": "conversation", "payload": { "headers": [
                { "name": "From", "value": "alice@example.com" },
                { "name": "Reply-To", "value": "team@example.com" },
                { "name": "Subject", "value": "Earlier subject" },
                { "name": "Message-ID", "value": "<older@example.com>" }
            ]}
        }))
        .unwrap();
        for reply_all in [false, true] {
            let reply = reply_message(&original, "sender@example.com", reply_all);
            assert_eq!(reply.in_reply_to.as_deref(), Some("<older@example.com>"));
            assert_eq!(reply.thread_id.as_deref(), Some("conversation"));
            assert_eq!(reply.to, "team@example.com");
            assert_eq!(reply.subject, "Re: Earlier subject");
        }
    }
}
