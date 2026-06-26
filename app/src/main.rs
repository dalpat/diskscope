//! DiskScope — a native GTK4 disk-usage analyzer.
//!
//! `main` does nothing but boot the GTK/libadwaita application and hand off to
//! [`ui::build_ui`]. All scanning logic lives in the `diskscope` library crate.

mod ui;

use adw::prelude::*;

const APP_ID: &str = "dev.diskscope.DiskScope";

fn main() -> gtk::glib::ExitCode {
    // Optional: a folder to scan immediately, e.g. `diskscope ~/Downloads`.
    let initial = std::env::args().nth(1);

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| ui::build_ui(app, initial.clone()));

    // Pass no args to GTK itself — we handle our own single path argument.
    app.run_with_args::<&str>(&[])
}
