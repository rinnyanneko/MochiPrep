//! The Settings window: the backend's port and starting MochiPrep at login.

use mochi_launcher_core::settings::{self, validate_port, LauncherSettings};
use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use crate::app::{self, lock, AppState};

pub const SETTINGS: &str = "settings";

#[derive(Serialize)]
pub struct SettingsView {
    port: u16,
    autostart: bool,
}

/// Validates the form and returns the settings to save, plus whether the port changed (which
/// needs a backend restart to take effect). A bad port leaves the settings as they are.
pub fn apply_settings(
    current: &LauncherSettings,
    port: &str,
    autostart: bool,
) -> Result<(LauncherSettings, bool), String> {
    let port = validate_port(port)?;
    let next = LauncherSettings {
        port,
        autostart,
        ..current.clone()
    };
    Ok((next, port != current.port))
}

/// Opens Settings, or focuses it if open.
pub fn open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, SETTINGS, WebviewUrl::App("settings.html".into()))
        .title("MochiPrep: Settings")
        .inner_size(460.0, 300.0)
        .resizable(false)
        .center()
        .build();
    if let Err(error) = built {
        app::show_error(
            app,
            "Cannot open Settings",
            &format!("MochiPrep cannot open the Settings window: {error}"),
        );
    }
}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> SettingsView {
    let settings = lock(&app.state::<AppState>().settings).clone();
    SettingsView {
        port: settings.port,
        autostart: settings.autostart,
    }
}

/// Saves the form. The login item is set before the file is written, so the file never claims
/// an autostart the system did not take; it is set every time, which also repairs a login item
/// removed outside MochiPrep. A new port only takes effect when the backend restarts, so the
/// owner is asked whether to restart now.
#[tauri::command]
pub async fn save_settings(app: AppHandle, port: String, autostart: bool) -> Result<(), String> {
    let (port_changed, new_port) = {
        let state = app.state::<AppState>();
        let mut current = lock(&state.settings);
        let (next, port_changed) = apply_settings(&current, &port, autostart)?;
        let login_item = if autostart {
            app.autolaunch().enable()
        } else {
            app.autolaunch().disable()
        };
        login_item.map_err(|error| format!("Cannot change starting at login: {error}"))?;
        settings::save_settings(&state.settings_path, &next)
            .map_err(|error| format!("Cannot save the settings: {error}"))?;
        let new_port = next.port;
        *current = next;
        (port_changed, new_port)
    };
    if port_changed {
        ask_restart(&app, new_port);
    }
    Ok(())
}

fn ask_restart(app: &AppHandle, port: u16) {
    let handle = app.clone();
    app.dialog()
        .message(format!("Restart the backend now to use port {port}?"))
        .title("Port changed")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::YesNo)
        .show(move |yes| {
            if yes {
                // The same Restart as the tray's, so a running agent or an armed real account
                // still asks before the backend stops.
                tauri::async_runtime::spawn(async move { app::restart(&handle).await });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> LauncherSettings {
        LauncherSettings {
            accepted_terms_version: Some(1),
            port: 8787,
            autostart: false,
            import_offered: true,
        }
    }

    #[test]
    fn a_bad_port_is_the_validation_error() {
        let expected = validate_port("80").unwrap_err();
        assert_eq!(apply_settings(&current(), "80", true), Err(expected));
        assert_eq!(
            apply_settings(&current(), "x", false),
            Err(validate_port("x").unwrap_err())
        );
    }

    #[test]
    fn the_same_port_is_not_a_change() {
        let (next, changed) = apply_settings(&current(), " 8787 ", true).unwrap();
        assert!(!changed);
        assert_eq!(
            next,
            LauncherSettings {
                autostart: true,
                ..current()
            }
        );
    }

    #[test]
    fn a_new_port_is_a_change_and_keeps_the_rest() {
        let (next, changed) = apply_settings(&current(), "8977", false).unwrap();
        assert!(changed);
        assert_eq!(
            next,
            LauncherSettings {
                port: 8977,
                ..current()
            }
        );
    }
}
