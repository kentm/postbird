mod accounts;
mod cache;
mod gmail;
mod oauth;
mod preferences;
mod ui;

use adw::prelude::*;

const APP_ID: &str = "io.github.postbird.Mail";

fn main() -> adw::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(ui::build);
    app.run()
}
