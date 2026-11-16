// A release build is a tray app: no console window behind it on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod tray;
mod windows;

use tauri::RunEvent;
use tauri_plugin_autostart::MacosLauncher;

fn main() {
    let launcher = tauri::Builder::default()
        // First, so a second launch exits before any other plugin or the setup runs.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            app::on_second_launch(app)
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            windows::accept_terms,
            windows::decline_terms,
            windows::close_terms
        ])
        .setup(|app| app::setup(app.handle()))
        .build(tauri::generate_context!())
        .expect("cannot build the MochiPrep launcher");
    launcher.run(|app, event| {
        // The launcher lives in the tray: closing its last window must not end it, only Quit.
        if let RunEvent::ExitRequested { api, .. } = event {
            if !app::quitting(app) {
                api.prevent_exit();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    const TERMS: &str = include_str!("../ui/terms.html");

    #[test]
    fn terms_are_version_1_with_the_exchange_status_line() {
        assert!(TERMS.contains("Version 1"));
        assert!(TERMS.contains(
            "The OKX integration in this version has been tested only against a simulated exchange, not against OKX Demo Trading or a real OKX account."
        ));
    }
}
