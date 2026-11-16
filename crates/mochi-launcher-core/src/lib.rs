//! Window-free logic of the desktop launcher: settings and terms consent, data import and the
//! tray menu model. Kept free of Tauri so it can be tested headless.

pub mod import;
pub mod model;
pub mod settings;
