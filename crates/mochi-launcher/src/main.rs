// A release build is a tray app: no console window behind it on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod import_window;
mod panel;
mod settings_window;
mod tray;
mod updater;
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            windows::accept_terms,
            windows::decline_terms,
            windows::close_terms,
            settings_window::get_settings,
            settings_window::save_settings,
            import_window::choose_import_folder,
            import_window::run_import,
            import_window::skip_import,
            panel::panel_state,
            panel::panel_action
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
    const SPEC: &str =
        include_str!("../../../docs/superpowers/specs/2026-11-07-rust-backend-desktop-design.md");
    const EXCHANGE_STATUS_LINE: &str = "The OKX integration in this version has been tested only against a simulated exchange, not against OKX Demo Trading or a real OKX account.";

    fn normalise_whitespace(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The text a reader sees in the page's `<main>`: tags removed, whitespace collapsed.
    fn page_text() -> String {
        let start = TERMS.find("<main").expect("terms page has a <main>");
        let end = TERMS.find("</main>").expect("terms page closes its <main>");
        let mut text = String::new();
        let mut in_tag = false;
        for c in TERMS[start..end].chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => text.push(c),
                _ => {}
            }
        }
        normalise_whitespace(&text)
    }

    /// The spec appendix's quoted block, as plain text: Markdown bold and the checkbox glyphs
    /// removed, the release's exchange status line filled in.
    fn appendix_text() -> String {
        let start = SPEC
            .find("> **Risk warning and disclaimer**")
            .expect("spec has the terms appendix");
        let quoted: Vec<&str> = SPEC[start..]
            .lines()
            .take_while(|line| line.starts_with('>'))
            .map(|line| line.trim_start_matches('>').trim())
            .collect();
        let text = quoted
            .join(" ")
            .replace("**", "")
            .replace('☐', "")
            .replace("{Exchange status line}", EXCHANGE_STATUS_LINE);
        normalise_whitespace(&text)
    }

    #[test]
    fn terms_are_version_1_with_the_exchange_status_line() {
        assert!(TERMS.contains("Version 1"));
        assert!(TERMS.contains(EXCHANGE_STATUS_LINE));
    }

    #[test]
    fn terms_page_is_the_spec_appendix_verbatim() {
        assert_eq!(page_text(), appendix_text());
    }
}
