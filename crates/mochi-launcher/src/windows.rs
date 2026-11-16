//! The launcher's windows: the terms page and the interface, plus the rule for where the
//! interface may navigate.

use mochi_launcher_core::settings::{save_settings, terms_accepted, TERMS_VERSION};
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

use crate::app::{self, lock, AppState};
use mochi_launcher_core::model::BackendState;

pub const TERMS: &str = "terms";
pub const INTERFACE: &str = "interface";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Navigation {
    Allow,
    OpenExternally,
    Deny,
}

/// Only the backend's own origin loads inside the interface window: the session token and the
/// page's saved preferences belong to exactly `http://127.0.0.1:<port>`, so even `localhost` on
/// the same port is another site. Other web links go to the system browser; any other scheme
/// (`file:`, `javascript:`, custom ones) is refused.
pub fn allowed_navigation(url: &Url, port: u16) -> Navigation {
    match url.scheme() {
        "http" if url.host_str() == Some("127.0.0.1") && url.port() == Some(port) => {
            Navigation::Allow
        }
        "http" | "https" => Navigation::OpenExternally,
        _ => Navigation::Deny,
    }
}

/// Opens the terms, or focuses them if open. Before acceptance they ask for consent; afterwards
/// the same page opens read-only, since there is nothing left to agree to.
pub fn open_terms(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(TERMS) {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let read_only = terms_accepted(&lock(&app.state::<AppState>().settings));
    let mut builder = WebviewWindowBuilder::new(app, TERMS, WebviewUrl::App("terms.html".into()))
        .title("MochiPrep: Risk warning and disclaimer")
        .inner_size(640.0, 720.0)
        .center();
    if read_only {
        builder = builder.initialization_script("window.__MOCHIPREP_TERMS_READ_ONLY__ = true;");
    }
    let window = match builder.build() {
        Ok(window) => window,
        Err(error) => {
            terms_window_failed(app, read_only, &error.to_string());
            return;
        }
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        // Closing the terms without accepting is declining them: there is nothing to run.
        if let WindowEvent::CloseRequested { .. } = event {
            if !terms_accepted(&lock(&handle.state::<AppState>().settings)) {
                app::exit_now(&handle);
            }
        }
    });
}

/// Without a terms window there is no way to accept the terms, so at first launch the launcher
/// explains and quits rather than sitting in the tray with a backend it may never start.
fn terms_window_failed(app: &AppHandle, read_only: bool, error: &str) {
    let message = format!("MochiPrep cannot open the risk warning window: {error}");
    if read_only {
        app::show_error(app, "Cannot open the risk warning", &message);
        return;
    }
    let handle = app.clone();
    app.dialog()
        .message(format!(
            "{message}. The backend was not started, and MochiPrep will now quit."
        ))
        .title("Cannot open the risk warning")
        .kind(MessageDialogKind::Error)
        .show(move |_| app::exit_now(&handle));
}

/// Opens the web interface on the running backend, or focuses it if open. The window is
/// destroyed when closed (Tauri's default), so no WebView runs while the interface is closed;
/// the backend keeps running.
pub fn open_interface(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(INTERFACE) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let app = app.clone();
    // The port is behind the async backend lock, which a start or stop may hold for a while.
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if *lock(&state.state) != BackendState::Running {
            return;
        }
        let port = match state.backend.lock().await.as_ref() {
            Some(backend) => backend.port(),
            None => return,
        };
        if let Err(error) = build_interface(&app, port) {
            // Two quick clicks race to build the window; the loser's failure is not an error.
            if app.get_webview_window(INTERFACE).is_none() {
                app::show_error(
                    &app,
                    "Cannot open the interface",
                    &format!("MochiPrep cannot open the interface window: {error}"),
                );
            }
        }
    });
}

fn build_interface(app: &AppHandle, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let url = Url::parse(&format!("http://127.0.0.1:{port}/"))?;
    let on_navigation = app.clone();
    let on_new_window = app.clone();
    WebviewWindowBuilder::new(app, INTERFACE, WebviewUrl::External(url))
        .title("MochiPrep")
        .inner_size(1400.0, 900.0)
        .center()
        .on_navigation(move |url| route(&on_navigation, url, port))
        // A second WebView would sit outside this window's navigation rule, so new-window
        // requests never create one: web links (`target="_blank"` news links) go to the
        // browser, and a link to the interface itself loads in this window.
        .on_new_window(move |url, _features| {
            if route(&on_new_window, &url, port) {
                let handle = on_new_window.clone();
                // Navigating from inside the WebView's own callback is deferred to a task, so
                // the WebView is not re-entered while it is still handling the request.
                tauri::async_runtime::spawn(async move {
                    if let Some(window) = handle.get_webview_window(INTERFACE) {
                        let _ = window.navigate(url);
                    }
                });
            }
            NewWindowResponse::Deny
        })
        .build()?;
    Ok(())
}

/// Applies `allowed_navigation`; returns whether the interface window may load `url` itself.
fn route(app: &AppHandle, url: &Url, port: u16) -> bool {
    match allowed_navigation(url, port) {
        Navigation::Allow => true,
        Navigation::OpenExternally => {
            let _ = app.opener().open_url(url.as_str(), None::<&str>);
            false
        }
        Navigation::Deny => false,
    }
}

/// Saves consent, then starts the backend. Consent that could not be saved is reported and the
/// backend is not started, so the next launch never runs on consent it does not remember.
#[tauri::command]
pub async fn accept_terms(app: AppHandle) -> Result<(), String> {
    {
        let state = app.state::<AppState>();
        let mut settings = lock(&state.settings);
        let mut next = settings.clone();
        next.accepted_terms_version = Some(TERMS_VERSION);
        save_settings(&state.settings_path, &next)
            .map_err(|error| format!("Cannot save your consent: {error}"))?;
        *settings = next;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move { app::start_backend(&handle).await });
    if let Some(window) = app.get_webview_window(TERMS) {
        let _ = window.destroy();
    }
    Ok(())
}

/// Quit on the terms page: nothing has been started, so there is nothing to stop.
#[tauri::command]
pub fn decline_terms(app: AppHandle) {
    app::exit_now(&app);
}

#[tauri::command]
pub fn close_terms(app: AppHandle) {
    if let Some(window) = app.get_webview_window(TERMS) {
        let _ = window.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(url: &str) -> Navigation {
        allowed_navigation(&Url::parse(url).unwrap(), 8787)
    }

    #[test]
    fn the_backend_origin_is_allowed() {
        assert_eq!(nav("http://127.0.0.1:8787/x"), Navigation::Allow);
    }

    #[test]
    fn localhost_is_another_origin() {
        assert_eq!(nav("http://localhost:8787/"), Navigation::OpenExternally);
    }

    #[test]
    fn another_port_is_another_origin() {
        assert_eq!(nav("http://127.0.0.1:9999/"), Navigation::OpenExternally);
    }

    #[test]
    fn web_links_open_externally() {
        assert_eq!(nav("https://news.example/a"), Navigation::OpenExternally);
    }

    #[test]
    fn other_schemes_are_denied() {
        assert_eq!(nav("file:///etc/passwd"), Navigation::Deny);
        assert_eq!(nav("javascript:alert(1)"), Navigation::Deny);
    }
}
