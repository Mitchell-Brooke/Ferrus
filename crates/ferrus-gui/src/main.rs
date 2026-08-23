mod app;
mod window;

use gtk::glib;

fn main() -> glib::ExitCode {
    app::run()
}
