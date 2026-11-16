//! The launcher's state and the flows that change it: start, stop, restart, quit, disarm, and
//! the poll that keeps the tray honest about a backend that died on its own.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use mochi_launcher_core::client::BackendClient;
use mochi_launcher_core::model::{
    menu_model, needs_confirmation, BackendState, SnapshotSlot, CONFIRM_TEXT, CONFIRM_TITLE,
};
use mochi_launcher_core::settings::{load_settings, terms_accepted, LauncherSettings};
use mochi_launcher_core::supervisor::{self, Backend, BackendCommand};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use crate::import_window::{self, should_offer_import, IMPORT};
use crate::panel::{self, PANEL};
use crate::tray::refresh_menu;
use crate::windows::{self, INTERFACE, TERMS};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const DIED_MESSAGE: &str = "backend stopped unexpectedly";

pub struct AppState {
    pub settings: Mutex<LauncherSettings>,
    pub settings_path: PathBuf,
    pub data_dir: PathBuf,
    /// A tokio mutex because starting and stopping hold it across awaits: whoever holds it owns
    /// the child, so a start and a stop can never interleave.
    pub backend: tokio::sync::Mutex<Option<Backend>>,
    pub state: Mutex<BackendState>,
    pub snapshot: Mutex<SnapshotSlot>,
    /// Set only by the Quit flows; any other exit request (the last window closing) is refused,
    /// because the launcher lives in the tray.
    pub quitting: AtomicBool,
    /// Set while a Restart or Quit flow runs, so repeated clicks do not queue a dialog each.
    pub stopping: AtomicBool,
    /// Set while a data import copies, so its window cannot be closed (which would start the
    /// backend into the folder being written).
    pub importing: AtomicBool,
}

/// A poisoned lock only means another thread panicked mid-update; the launcher's values are
/// plain data that stay usable, and refusing to read them would leave the tray stuck.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let settings_path = app.path().app_config_dir()?.join("launcher.json");
    let settings = load_settings(&settings_path);
    let data_dir = match std::env::var_os("MOCHIPREP_DATA_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => app.path().app_data_dir()?,
    };
    let accepted = terms_accepted(&settings);
    #[cfg(target_os = "macos")]
    macos_tray_only(app)?;
    app.manage(AppState {
        settings: Mutex::new(settings),
        settings_path,
        data_dir,
        backend: tokio::sync::Mutex::new(None),
        state: Mutex::new(BackendState::Stopped),
        snapshot: Mutex::new(SnapshotSlot::default()),
        quitting: AtomicBool::new(false),
        stopping: AtomicBool::new(false),
        importing: AtomicBool::new(false),
    });
    if panel::tray_available() {
        crate::tray::create(app)?;
    } else {
        panel::open_panel(app)?;
    }
    if accepted {
        let handle = app.clone();
        tauri::async_runtime::spawn(async move { start_backend(&handle).await });
    } else {
        windows::open_terms(app);
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move { poll(handle).await });
    Ok(())
}

/// On macOS the Dock's Quit and Cmd+Q call `terminate:`, which ends the process at once: no
/// confirmation, and no chance to stop the backend cleanly. A tray-only app has neither: no Dock
/// icon (the accessory policy), and a menu without Quit in place of Tauri's default one. The
/// Edit items stay, because the interface's copy and paste shortcuts are menu key equivalents
/// on macOS. Quit stays in the tray, where it asks first.
#[cfg(target_os = "macos")]
fn macos_tray_only(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, SubmenuBuilder};

    app.set_activation_policy(tauri::ActivationPolicy::Accessory)?;
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    app.set_menu(MenuBuilder::new(app).item(&edit).build()?)?;
    Ok(())
}

pub fn quitting(app: &AppHandle) -> bool {
    app.try_state::<AppState>()
        .is_some_and(|state| state.quitting.load(Ordering::SeqCst))
}

/// Exits without touching the backend; only for when none was started (terms declined).
pub fn exit_now(app: &AppHandle) {
    app.state::<AppState>()
        .quitting
        .store(true, Ordering::SeqCst);
    app.exit(0);
}

/// A second launch must not start a second backend: it brings up what the first one shows.
pub fn on_second_launch(app: &AppHandle) {
    let running = *lock(&app.state::<AppState>().state) == BackendState::Running;
    if running {
        windows::open_interface(app);
    } else if let Some(window) = app
        .get_webview_window(TERMS)
        .or_else(|| app.get_webview_window(IMPORT))
        .or_else(|| app.get_webview_window(PANEL))
    {
        let _ = window.unminimize();
        let _ = window.set_focus();
    } else {
        // Without this the second launch would look like it did nothing at all.
        let state = app.state::<AppState>();
        let backend = lock(&state.state).clone();
        let status = menu_model(&backend, None).status_label;
        show_error(
            app,
            "MochiPrep is already running",
            &format!("{status}. Use the MochiPrep tray icon to restart the backend or quit."),
        );
    }
}

/// A native error dialog; it needs no window, so it works when the launcher's own windows
/// could not be created.
pub fn show_error(app: &AppHandle, title: &str, message: &str) {
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

/// The backend sits next to the launcher: both build into the same target directory in
/// development, and an installer puts them in one folder.
fn backend_program() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("cannot locate the launcher: {error}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "cannot locate the launcher's folder".to_string())?;
    let program = dir.join(format!("mochi-server{}", std::env::consts::EXE_SUFFIX));
    if program.is_file() {
        Ok(program)
    } else {
        Err(format!(
            "backend program not found at {}",
            program.display()
        ))
    }
}

/// Offline is a development and test switch: it is read from the launcher's own environment and
/// never stored in the settings, so an installed launcher always runs online.
fn offline() -> bool {
    std::env::var("MOCHIPREP_OFFLINE").as_deref() == Ok("1")
}

fn set_state(app: &AppHandle, next: BackendState) {
    let state = app.state::<AppState>();
    *lock(&state.state) = next;
    lock(&state.snapshot).reset();
    refresh_menu(app);
}

/// Starts the backend unless one is alive. Never before the terms are accepted, and never while
/// the one-time import is still unanswered (a backend would create the database the import
/// needs to write) or a copy is running: all are checked here, not by the callers, so no menu
/// item or second launch can get around them. The import clears `importing` under the settings
/// lock in the same step that settles the offer, so reading both under it sees either an import
/// still pending or one fully settled.
pub async fn start_backend(app: &AppHandle) {
    let state = app.state::<AppState>();
    let (accepted, offer_import, port) = {
        let settings = lock(&state.settings);
        (
            terms_accepted(&settings),
            state.importing.load(Ordering::SeqCst)
                || should_offer_import(&settings, &state.data_dir),
            settings.port,
        )
    };
    if !accepted {
        windows::open_terms(app);
        return;
    }
    if offer_import {
        import_window::open_import(app);
        return;
    }
    let mut backend = state.backend.lock().await;
    if backend.as_mut().is_some_and(|running| running.is_alive()) {
        return;
    }
    *backend = None;
    set_state(app, BackendState::Starting);
    let started = match backend_program() {
        Ok(program) => {
            let command = BackendCommand {
                program,
                port,
                data_dir: state.data_dir.clone(),
                offline: offline(),
            };
            supervisor::start(&command)
                .await
                .map_err(|error| error.to_string())
        }
        Err(message) => Err(message),
    };
    let next = match started {
        Ok(child) => {
            *backend = Some(child);
            BackendState::Running
        }
        Err(message) => BackendState::Error(message),
    };
    // Published while the child is still owned: a stop that takes the lock next then sets its
    // own state after this one, never before it.
    set_state(app, next);
    drop(backend);
    refresh_snapshot(app).await;
}

/// Stops the backend if there is one. Holding the backend lock for the whole stop keeps a
/// start from racing it onto the same port.
pub async fn stop_backend(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut backend = state.backend.lock().await;
    let Some(child) = backend.take() else {
        return;
    };
    set_state(app, BackendState::Stopped);
    child.stop().await;
}

/// Asks before anything that stops a backend which may be running agents or holding armed real
/// accounts. The snapshot is read fresh: the poll's copy can be 3 s old, and an unreadable
/// state with a live process asks too.
pub async fn confirm_stop(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    // A start holds the backend for up to 20 s while its process comes up. That process is
    // alive and its state not yet readable, which needs the confirmation anyway, so it is asked
    // now instead of after a silent wait; the stop that follows waits for the start. Checked
    // before the lock, so whatever the lock shows at this moment cannot skip the question.
    if *lock(&state.state) == BackendState::Starting {
        return ask_to_stop(app).await;
    }
    let held = match state.backend.try_lock() {
        Ok(mut backend) => Some(child_status(&mut backend)),
        Err(_) => None,
    };
    let (alive, port) = match held {
        Some(status) => status,
        None if *lock(&state.state) == BackendState::Starting => return ask_to_stop(app).await,
        None => child_status(&mut *state.backend.lock().await),
    };
    let snapshot = match (alive, port) {
        (true, Some(port)) => BackendClient::new(port).snapshot().await.ok(),
        _ => None,
    };
    let current = lock(&state.state).clone();
    if !needs_confirmation(&current, alive, snapshot.as_ref()) {
        return true;
    }
    ask_to_stop(app).await
}

fn child_status(backend: &mut Option<Backend>) -> (bool, Option<u16>) {
    match backend.as_mut() {
        Some(child) => (child.is_alive(), Some(child.port())),
        None => (false, None),
    }
}

async fn ask_to_stop(app: &AppHandle) -> bool {
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(CONFIRM_TEXT)
        .title(CONFIRM_TITLE)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Continue".to_string(),
            "Cancel".to_string(),
        ))
        .show(move |ok| {
            let _ = answer_tx.send(ok);
        });
    // A dialog that vanished without an answer counts as Cancel.
    answer_rx.await.unwrap_or(false)
}

/// Claims the single Restart/Quit slot; false if another flow already holds it.
fn begin_stop_flow(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .stopping
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

fn end_stop_flow(app: &AppHandle) {
    app.state::<AppState>()
        .stopping
        .store(false, Ordering::SeqCst);
}

/// Later Restart clicks are ignored while one Restart or Quit is in progress.
pub async fn restart(app: &AppHandle) {
    if !begin_stop_flow(app) {
        return;
    }
    restart_flow(app).await;
    end_stop_flow(app);
}

async fn restart_flow(app: &AppHandle) {
    if !confirm_stop(app).await {
        return;
    }
    // The page would only show connection errors while the backend is down, and its session
    // token dies with the old process.
    if let Some(interface) = app.get_webview_window(INTERFACE) {
        let _ = interface.destroy();
    }
    stop_backend(app).await;
    start_backend(app).await;
}

/// Later Quit clicks are ignored while one Restart or Quit is in progress.
pub async fn quit(app: &AppHandle) {
    if !begin_stop_flow(app) {
        return;
    }
    if !confirm_stop(app).await {
        end_stop_flow(app);
        return;
    }
    app.state::<AppState>()
        .quitting
        .store(true, Ordering::SeqCst);
    stop_backend(app).await;
    app.exit(0);
}

pub async fn disarm_all(app: &AppHandle) {
    let Some(port) = running_port(app).await else {
        return;
    };
    if let Err(error) = BackendClient::new(port).disarm_all_live().await {
        show_error(
            app,
            "Disarm all failed",
            &format!(
                "Some real accounts may still be armed: {error}. Check them in the interface."
            ),
        );
    }
    refresh_snapshot(app).await;
}

async fn running_port(app: &AppHandle) -> Option<u16> {
    let state = app.state::<AppState>();
    let backend = state.backend.lock().await;
    backend.as_ref().map(|child| child.port())
}

/// Reads agents and armed accounts for the tray. Skips when a start or stop owns the backend:
/// that flow updates the menu itself.
async fn refresh_snapshot(app: &AppHandle) {
    let state = app.state::<AppState>();
    // Taken first: any state change from here on outdates the answer.
    let generation = lock(&state.snapshot).generation();
    if *lock(&state.state) != BackendState::Running {
        return;
    }
    let port = {
        let Ok(mut backend) = state.backend.try_lock() else {
            return;
        };
        let Some(child) = backend.as_mut() else {
            return;
        };
        if !child.is_alive() {
            // No automatic restart: a backend that keeps dying would loop, and the owner
            // should see that it stopped, and why (the backend redacts its own logs).
            let tail = child.stderr_tail();
            *backend = None;
            // Published while the lock is held, as start and stop do: a start waiting for the
            // lock then sets its own state after this one, never before it.
            set_state(app, BackendState::Error(died_message(&tail)));
            return;
        }
        child.port()
    };
    let snapshot = BackendClient::new(port).snapshot().await.ok();
    // A stop, or a whole restart, may have happened while the request was out; then the answer
    // is about the old process and is dropped.
    let changed = lock(&state.snapshot).store(generation, snapshot);
    // Rebuilding the menu closes it if it is open, so only a real change does.
    if changed {
        refresh_menu(app);
    }
}

/// A tray label is one line, so the tail's whitespace is collapsed and only its end is kept: the
/// error is usually the last thing the backend wrote.
fn died_message(stderr_tail: &str) -> String {
    const MAX_CHARS: usize = 200;
    let words: Vec<&str> = stderr_tail.split_whitespace().collect();
    if words.is_empty() {
        return DIED_MESSAGE.to_string();
    }
    let line = words.join(" ");
    let count = line.chars().count();
    let shown = if count > MAX_CHARS {
        let end: String = line.chars().skip(count - MAX_CHARS).collect();
        format!("...{end}")
    } else {
        line
    };
    format!("{DIED_MESSAGE}: {shown}")
}

async fn poll(app: AppHandle) {
    let mut ticks = tokio::time::interval(POLL_INTERVAL);
    // A slow poll (a hung backend takes 5 s per request) must not be followed by a burst of
    // catch-up ticks.
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        refresh_snapshot(&app).await;
    }
}

#[cfg(test)]
mod tests {
    use super::died_message;

    #[test]
    fn died_message_keeps_the_end_of_the_tail_on_one_line() {
        assert_eq!(died_message(" \n"), "backend stopped unexpectedly");
        assert_eq!(
            died_message("starting\nError: database is locked\n\n"),
            "backend stopped unexpectedly: starting Error: database is locked"
        );
        let long = format!("{}\nthe reason", "x".repeat(500));
        let message = died_message(&long);
        assert!(message.ends_with("the reason"));
        assert!(message.starts_with("backend stopped unexpectedly: ..."));
        assert_eq!(
            message.chars().count(),
            "backend stopped unexpectedly: ...".len() + 200
        );
    }
}
