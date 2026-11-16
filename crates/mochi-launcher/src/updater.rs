//! Looks for a new release and, when the owner clicks the tray's update item, installs it. The
//! checks only find out what exists: nothing is downloaded or installed without that click.

use std::time::{Duration, Instant};

use mochi_launcher_core::update::{
    check_due, install_kind, update_steps, InstallKind, UpdateOffer, UpdateStep, RELEASES_PAGE,
};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::app::{self, lock, AppState};
use crate::tray::refresh_menu;
use crate::windows::INTERFACE;

/// How often the loop asks `check_due`. A day between checks is the rule; the shorter tick only
/// means a `checkUpdates` switched on in Settings is honoured within the hour.
const TICK: Duration = Duration::from_secs(60 * 60);

/// What the last check found: the label's data, and the plugin's handle to download it.
pub struct Available {
    pub offer: UpdateOffer,
    update: Update,
}

/// Starts the check loop. Debug builds never check: a developer's build would always look older
/// than the latest release, and a click would replace it with that release.
pub fn start(app: &AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move { check_loop(handle).await });
}

async fn check_loop(app: AppHandle) {
    let mut last_attempt: Option<Instant> = None;
    loop {
        let enabled = lock(&app.state::<AppState>().settings).check_updates;
        if check_due(enabled, last_attempt, Instant::now()) {
            last_attempt = Some(Instant::now());
            check(&app).await;
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn check(app: &AppHandle) {
    let found = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(error) => Err(error),
    };
    let available = match found {
        Ok(Some(update)) => Some(Available {
            offer: UpdateOffer {
                version: update.version.clone(),
                kind: this_install(),
            },
            update,
        }),
        Ok(None) => None,
        Err(error) => {
            // No dialog: the owner did not ask for this check, and a failed one is tried again a
            // day later. The endpoint is public and carries no token.
            eprintln!("update check failed: {error}");
            return;
        }
    };
    *lock(&app.state::<AppState>().update) = available;
    refresh_menu(app);
}

fn this_install() -> InstallKind {
    install_kind(
        std::env::consts::OS,
        std::env::var_os("APPIMAGE").as_deref(),
    )
}

/// The tray's update item. A package-managed install only opens the release page; any other
/// install follows `update_steps`, after the same confirmation as Restart and Quit.
pub async fn apply_update(app: &AppHandle) {
    let Some((kind, update)) = lock(&app.state::<AppState>().update)
        .as_ref()
        .map(|available| (available.offer.kind, available.update.clone()))
    else {
        return;
    };
    if kind == InstallKind::PackageManaged {
        if let Err(error) = app.opener().open_url(RELEASES_PAGE, None::<&str>) {
            app::show_error(
                app,
                "Cannot open the release page",
                &format!("Open {RELEASES_PAGE} in a browser to get the new version: {error}"),
            );
        }
        return;
    }
    // The update stops the backend, so it takes the same single slot as Restart and Quit: none
    // of them can run while it does, and a second click is ignored.
    if !app::begin_stop_flow(app) {
        return;
    }
    if app::confirm_stop(app).await {
        run_steps(app, &update).await;
    }
    app::end_stop_flow(app);
}

/// Each step either succeeds or ends the update with the backend running again: a failed
/// download has stopped nothing, and a failed install starts the backend it stopped.
async fn run_steps(app: &AppHandle, update: &Update) {
    let mut bytes = Vec::new();
    for step in update_steps() {
        match step {
            UpdateStep::Download => match update.download(|_, _| {}, || {}).await {
                Ok(downloaded) => bytes = downloaded,
                Err(error) => {
                    app::show_error(
                        app,
                        "Update failed",
                        &format!(
                            "MochiPrep could not download v{}: {error}. Nothing was changed.",
                            update.version
                        ),
                    );
                    return;
                }
            },
            UpdateStep::StopBackend => {
                // As for Restart: the page would only show connection errors, and its session
                // token dies with the old process.
                if let Some(interface) = app.get_webview_window(INTERFACE) {
                    let _ = interface.destroy();
                }
                app::stop_backend(app).await;
            }
            // On Windows a successful install starts the installer and ends this process, so
            // the steps after it run only on macOS and Linux.
            UpdateStep::Install => {
                if let Err(error) = update.install(&bytes) {
                    app::start_backend(app).await;
                    app::show_error(
                        app,
                        "Update failed",
                        &format!(
                            "MochiPrep could not install v{}: {error}. The current version keeps running.",
                            update.version
                        ),
                    );
                    return;
                }
            }
            UpdateStep::Relaunch => {
                app::mark_quitting(app);
                app.restart();
            }
        }
    }
}
