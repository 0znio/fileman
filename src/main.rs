//! A fast GTK4 file manager.

mod app;
mod archive;
mod config;
mod drives;
mod fs;
mod history;
mod trace;
mod ui;

fn main() -> gtk::glib::ExitCode {
    app::run()
}
