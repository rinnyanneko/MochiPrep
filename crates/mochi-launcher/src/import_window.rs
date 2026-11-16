//! The one-time offer, between the terms and the first backend start, to import an existing
//! `.mochiprep` folder (the Node backend's data) instead of starting empty.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use mochi_launcher_core::import::import_data;
use mochi_launcher_core::settings::{self, LauncherSettings};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

use crate::app::{self, lock, AppState};

pub const IMPORT: &str = "import";
const DB_NAME: &str = "mochiprep.db";

/// Offered once, and only while the data dir has no database: importing into existing data is
/// refused anyway, so asking then would only lead to an error.
pub fn should_offer_import(settings: &LauncherSettings, data_dir: &Path) -> bool {
    !settings.import_offered && !data_dir.join(DB_NAME).exists()
}

/// Opens the import offer, or focuses it if open.
pub fn open_import(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(IMPORT) {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, IMPORT, WebviewUrl::App("import.html".into()))
        .title("MochiPrep: Import existing data")
        .inner_size(520.0, 240.0)
        .resizable(false)
        .center()
        .build();
    let window = match built {
        Ok(window) => window,
        Err(error) => {
            // Without the window there is nothing to answer, so the offer is settled as a Skip
            // and the backend starts on an empty data dir.
            app::show_error(
                app,
                "Cannot open the import window",
                &format!("MochiPrep cannot open the import window, so it starts without importing: {error}"),
            );
            finish(app);
            return;
        }
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            // A copy in progress must finish (or fail) first; a backend starting meanwhile would
            // create its own database in the target.
            if handle.state::<AppState>().importing.load(Ordering::SeqCst) {
                api.prevent_close();
                return;
            }
            // Closing the window is a Skip.
            finish(&handle);
        }
    });
}

/// Settles the offer and starts the backend. A failed save is not reported: the cost is only
/// that the next launch offers again, and only if the data dir still has no database.
fn finish(app: &AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut current = lock(&state.settings);
        let mut next = current.clone();
        next.import_offered = true;
        let _ = settings::save_settings(&state.settings_path, &next);
        *current = next;
    }
    if let Some(window) = app.get_webview_window(IMPORT) {
        let _ = window.destroy();
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move { app::start_backend(&handle).await });
}

#[tauri::command]
pub async fn choose_import_folder(app: AppHandle) -> Option<String> {
    let mut dialog = app.dialog().file().set_title("Choose a .mochiprep folder");
    if let Some(window) = app.get_webview_window(IMPORT) {
        dialog = dialog.set_parent(&window);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog.pick_folder(move |folder| {
        let _ = tx.send(folder);
    });
    rx.await.ok().flatten().map(|folder| folder.to_string())
}

/// Copies the chosen folder's database into the data dir. An error is returned for the window to
/// show, and the offer stays open so another folder can be tried or the import skipped.
#[tauri::command]
pub async fn run_import(app: AppHandle, folder: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state
        .importing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("An import is already running.".to_string());
    }
    let source = PathBuf::from(folder);
    let target = state.data_dir.clone();
    let copied = tauri::async_runtime::spawn_blocking(move || import_data(&source, &target)).await;
    state.importing.store(false, Ordering::SeqCst);
    match copied {
        Ok(Ok(())) => {
            finish(&app);
            Ok(())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(format!("Import failed: {error}")),
    }
}

#[tauri::command]
pub async fn skip_import(app: AppHandle) {
    finish(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(import_offered: bool) -> LauncherSettings {
        LauncherSettings {
            import_offered,
            ..Default::default()
        }
    }

    #[test]
    fn offered_into_an_empty_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(should_offer_import(&settings(false), dir.path()));
    }

    #[test]
    fn not_offered_twice() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!should_offer_import(&settings(true), dir.path()));
    }

    #[test]
    fn not_offered_over_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DB_NAME), b"").unwrap();
        assert!(!should_offer_import(&settings(false), dir.path()));
    }
}
