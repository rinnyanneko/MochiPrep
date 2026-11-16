fn main() {
    // Listing the commands makes Tauri generate a permission per command, so only windows whose
    // capability grants it can call one; the interface window has none.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&["accept_terms", "decline_terms", "close_terms"]),
    ))
    .expect("tauri build step failed");
}
