//! Application bootstrap.

use adw::prelude::*;
use adw::Application;
use gtk::glib;

pub const APP_ID: &str = "io.github.ferrus.Ferrus";

pub fn run() -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::empty())
        .build();

    app.connect_activate(|_app| {
        // Dark-mode-first, like modern GNOME utilities.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let win = crate::window::Window::new(_app);
        win.present_root();
    });

    app.run()
}
