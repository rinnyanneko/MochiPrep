//! The tray icon and its menu, rebuilt from `menu_model` whenever the backend state changes.

use mochi_launcher_core::model::{menu_model, MenuModel};
use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{include_image, AppHandle, Manager};

use crate::app::{self, lock, AppState};
use crate::windows;

const TRAY_ID: &str = "main";
const TOOLTIP: &str = "MochiPrep";
const TOOLTIP_ARMED: &str = "MochiPrep — real account armed";

const OPEN: &str = "open";
const RESTART: &str = "restart";
const DISARM: &str = "disarm";
const TERMS: &str = "terms";
const QUIT: &str = "quit";

fn icon(armed: bool) -> Image<'static> {
    if armed {
        include_image!("icons/tray-armed.png")
    } else {
        include_image!("icons/tray.png")
    }
}

fn current_model(app: &AppHandle) -> MenuModel {
    let state = app.state::<AppState>();
    let backend = lock(&state.state).clone();
    let snapshot = lock(&state.snapshot).clone();
    menu_model(&backend, snapshot.as_ref())
}

fn build_menu(app: &AppHandle, model: &MenuModel) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(
        app,
        "status",
        &model.status_label,
        false,
        None::<&str>,
    )?)?;
    if let Some(armed) = &model.armed_label {
        menu.append(&MenuItem::with_id(
            app,
            "armed",
            armed,
            false,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        OPEN,
        "Open interface",
        model.can_open_interface,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        RESTART,
        "Restart backend",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        DISARM,
        "Disarm all",
        model.can_disarm_all,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        TERMS,
        "Risk & disclaimer…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(app, QUIT, "Quit", true, None::<&str>)?)?;
    Ok(menu)
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let model = current_model(app);
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(false))
        .tooltip(TOOLTIP)
        .menu(&build_menu(app, &model)?)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu_event)
        .build(app)?;
    Ok(())
}

/// Rebuilds the menu and swaps the icon so an armed real account is visible without opening
/// the menu. Failures are ignored: the next state change rebuilds it again.
pub fn refresh_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let model = current_model(app);
    if let Ok(menu) = build_menu(app, &model) {
        let _ = tray.set_menu(Some(menu));
    }
    let armed = model.armed_label.is_some();
    let _ = tray.set_icon(Some(icon(armed)));
    let _ = tray.set_tooltip(Some(if armed { TOOLTIP_ARMED } else { TOOLTIP }));
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let handle = app.clone();
    match event.id().as_ref() {
        OPEN => windows::open_interface(app),
        TERMS => windows::open_terms(app),
        RESTART => {
            tauri::async_runtime::spawn(async move { app::restart(&handle).await });
        }
        DISARM => {
            tauri::async_runtime::spawn(async move { app::disarm_all(&handle).await });
        }
        QUIT => {
            tauri::async_runtime::spawn(async move { app::quit(&handle).await });
        }
        _ => {}
    }
}
