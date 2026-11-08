// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod args;
mod engine;
#[cfg(test)]
mod engine_tests;
mod history;
mod logfile;
mod progress;
mod system;
mod taskbar;

fn main() -> Result<(), slint::PlatformError> {
    // Same folder earlier versions used (%APPDATA%\batchviproc), so processed history carries over.
    let data_dir = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("batchviproc");
    let _ = std::fs::create_dir_all(&data_dir);
    logfile::install_panic_hook(data_dir.join("logs"));

    // Two instances would append to the same history and fight over the same files.
    let Some(_instance) = system::SingleInstance::acquire(r"Local\batchviproc-single-instance") else {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Info)
            .set_title("batchviproc")
            .set_description("batchviproc is already running.")
            .show();
        return Ok(());
    };

    app::run(data_dir, app::Launch::from_args(std::env::args().skip(1)))
}
