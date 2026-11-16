//! The fallback panel: a small window with the tray's items, for desktops that show no tray
//! icons (GNOME without the AppIndicator extension). Without it the launcher would run with no
//! visible way to open the interface or quit.

use mochi_launcher_core::model::MenuModel;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::app;
use crate::tray;

pub const PANEL: &str = "panel";
const STATE_EVENT: &str = "panel-state";

/// Linux trays are the StatusNotifierItem protocol: an icon only shows if some process owns the
/// watcher name on the session bus. Other systems always have a tray.
#[cfg(target_os = "linux")]
pub fn tray_available() -> bool {
    use std::time::Duration;

    const WATCHER: &str = "org.kde.StatusNotifierWatcher";

    fn watcher_has_owner() -> Option<bool> {
        let connection = zbus::blocking::Connection::session().ok()?;
        let proxy = zbus::blocking::fdo::DBusProxy::new(&connection).ok()?;
        let name = zbus::names::BusName::try_from(WATCHER).ok()?;
        proxy.name_has_owner(name).ok()
    }

    // zbus has no connect timeout, and a stuck bus must not hold up the launch: the check runs
    // on its own thread, and no answer within a second counts as no tray.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(watcher_has_owner());
    });
    rx.recv_timeout(Duration::from_secs(1))
        .ok()
        .flatten()
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
pub fn tray_available() -> bool {
    true
}

/// What the panel page draws: the tray menu's labels and enabled items.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelView {
    status: String,
    armed: Option<String>,
    can_open_interface: bool,
    can_disarm_all: bool,
}

impl From<&MenuModel> for PanelView {
    fn from(model: &MenuModel) -> Self {
        Self {
            status: model.status_label.clone(),
            armed: model.armed_label.clone(),
            can_open_interface: model.can_open_interface,
            can_disarm_all: model.can_disarm_all,
        }
    }
}

pub fn open_panel(app: &AppHandle) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, PANEL, WebviewUrl::App("panel.html".into()))
        .title("MochiPrep")
        .inner_size(320.0, 360.0)
        .resizable(false)
        .center()
        .build()?;
    let handle = app.clone();
    window.on_window_event(move |event| {
        // The panel stands in for the tray, so closing it is Quit; the Quit flow exits the app
        // or, if the owner cancels its confirmation, leaves the panel open.
        if let WindowEvent::CloseRequested { api, .. } = event {
            if !app::quitting(&handle) {
                api.prevent_close();
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move { app::quit(&handle).await });
            }
        }
    });
    Ok(())
}

/// Pushes the current state to the panel; called wherever the tray menu is rebuilt.
pub fn refresh_panel(app: &AppHandle, model: &MenuModel) {
    if app.get_webview_window(PANEL).is_some() {
        let _ = app.emit_to(PANEL, STATE_EVENT, PanelView::from(model));
    }
}

/// The state when the page loads; later changes arrive as events.
#[tauri::command]
pub fn panel_state(app: AppHandle) -> PanelView {
    PanelView::from(&tray::current_model(&app))
}

/// Async so that it runs off the main thread: some actions build a window, which a synchronous
/// command must not do on Windows.
#[tauri::command]
pub async fn panel_action(app: AppHandle, action: String) {
    tray::run_action(&app, &action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn the_tray_is_available_outside_linux() {
        assert!(tray_available());
    }
}
