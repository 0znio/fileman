//! A fast GTK4 file manager.

mod app;
mod archive;
mod config;
mod drives;
mod fs;
mod history;
// Shared with the `fileman-portal` binary, which uses the parts the
// application does not and the other way round.
#[allow(dead_code)]
mod portal;
#[cfg(test)]
mod testing;
mod trace;
mod ui;

fn main() -> gtk::glib::ExitCode {
    // The portal daemon starts us like this to draw one dialog and exit. It
    // is checked before anything else because the normal path forwards to an
    // already-running Fileman, and a file chooser must be its own process with
    // its own answer to give back.
    if std::env::args().any(|arg| arg == "--file-chooser") {
        return ui::chooser::run_portal_dialog();
    }
    app::run()
}
