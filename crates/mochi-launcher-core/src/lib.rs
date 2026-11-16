//! Window-free logic of the desktop launcher: settings and terms consent, data import, the tray
//! menu model, and the supervisor and HTTP client for the backend child. Kept free of Tauri so
//! it can be tested headless.

pub mod client;
pub mod import;
pub mod model;
pub mod settings;
pub mod supervisor;
