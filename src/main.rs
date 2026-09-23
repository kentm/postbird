mod accounts;
mod cache;
mod compose;
mod contacts;
mod gmail;
mod mail;
mod mailbox_sync;
mod preferences;
mod rich_editor;
mod theme;
mod ui;

use adw::prelude::*;

const APP_ID: &str = "io.github.postbird.Mail";

fn main() -> adw::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| theme::install_omarchy_integration());
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else {
            ui::build(app);
        }
    });
    app.run()
}
