fn main() {
    // Listing the commands makes Tauri generate a permission per command, so only windows whose
    // capability grants it can call one.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "accept_terms",
            "decline_terms",
            "close_terms",
            "get_settings",
            "save_settings",
            "choose_import_folder",
            "run_import",
            "skip_import",
            "panel_state",
            "panel_action",
            "interface_status",
            "interface_quit",
        ]),
    ))
    .expect("tauri build step failed");
}
