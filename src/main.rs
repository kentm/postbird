mod accounts;
mod cache;
mod compose;
mod contacts;
mod gmail;
mod mail;
mod mailbox_sync;
mod mailto;
mod preferences;
mod rich_editor;
mod theme;
mod ui;

use adw::prelude::*;

const APP_ID: &str = "io.github.postbird.Mail";

fn main() -> adw::glib::ExitCode {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|_| theme::install_omarchy_integration());
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else {
            ui::build(app);
        }
    });
    app.connect_open(|app, files, _| {
        // GApplication forwards open requests to the running instance, or
        // builds the window (and loads accounts) before the first request.
        app.activate();
        if let Some(window) = app.active_window() {
            for file in files {
                if let Err(error) =
                    window.activate_action("win.compose-mailto", Some(&file.uri().to_variant()))
                {
                    eprintln!("could not open mail link: {error}");
                }
            }
        }
    });
    app.run()
}
