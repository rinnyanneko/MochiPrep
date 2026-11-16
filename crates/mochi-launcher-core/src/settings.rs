//! The launcher's own settings and terms consent, kept in `launcher.json` outside the backend's
//! SQLite so consent exists before any backend runs.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bump when the terms text changes; stored consent for another version does not count.
pub const TERMS_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 8787;
const PORT_ERROR: &str = "Port must be a number from 1024 to 65535";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LauncherSettings {
    pub accepted_terms_version: Option<u32>,
    #[serde(deserialize_with = "lenient_port")]
    pub port: u16,
    pub autostart: bool,
    pub import_offered: bool,
    /// Whether the launcher looks for a new release by itself. Nothing is downloaded until the
    /// owner clicks the update item.
    pub check_updates: bool,
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            accepted_terms_version: None,
            port: DEFAULT_PORT,
            autostart: false,
            import_offered: false,
            check_updates: true,
        }
    }
}

/// An out-of-range or malformed port resets only the port, so a bad edit never costs the consent.
fn lenient_port<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(v.as_i64()
        .and_then(|n| u16::try_from(n).ok())
        .filter(|p| *p >= 1024)
        .unwrap_or(DEFAULT_PORT))
}

/// A missing, unreadable or corrupt file yields defaults, which means terms not accepted: consent
/// is never assumed.
pub fn load_settings(path: &Path) -> LauncherSettings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return LauncherSettings::default();
    };
    let Ok(s) = serde_json::from_str::<LauncherSettings>(&text) else {
        return LauncherSettings::default();
    };
    s
}

/// Writes a temp file beside the target, flushes it to disk and renames it over the target, so a
/// crash of the launcher never leaves a half-written consent file. The directory entry is not
/// synced, so after a power loss the old file, or on some file systems an empty one, may come
/// back; either loads as defaults or as the previous consent, never as an invented acceptance.
pub fn save_settings(path: &Path, s: &LauncherSettings) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(s).map_err(std::io::Error::other)?;
    let result = write_synced(&tmp, body.as_bytes()).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_synced(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(body)?;
    file.sync_all()
}

pub fn terms_accepted(s: &LauncherSettings) -> bool {
    s.accepted_terms_version == Some(TERMS_VERSION)
}

pub fn validate_port(text: &str) -> Result<u16, String> {
    match text.trim().parse::<u16>() {
        Ok(p) if p >= 1024 => Ok(p),
        _ => Err(PORT_ERROR.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_default_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let s = load_settings(&dir.path().join("launcher.json"));
        assert_eq!(s, LauncherSettings::default());
        assert!(!terms_accepted(&s));
    }

    #[test]
    fn corrupt_file_is_default_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("launcher.json");
        std::fs::write(&p, "{not json").unwrap();
        let s = load_settings(&p);
        assert_eq!(s, LauncherSettings::default());
        assert!(!terms_accepted(&s));
    }

    fn with_version(v: u32) -> LauncherSettings {
        LauncherSettings {
            accepted_terms_version: Some(v),
            ..Default::default()
        }
    }

    #[test]
    fn older_terms_version_is_not_accepted() {
        assert!(!terms_accepted(&with_version(0)));
    }

    #[test]
    fn newer_terms_version_is_not_accepted() {
        assert!(!terms_accepted(&with_version(2)));
    }

    #[test]
    fn current_version_is_accepted() {
        assert!(terms_accepted(&with_version(TERMS_VERSION)));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("launcher.json");
        std::fs::write(
            &p,
            r#"{"acceptedTermsVersion":1,"port":9000,"future":true}"#,
        )
        .unwrap();
        let s = load_settings(&p);
        assert_eq!(s.accepted_terms_version, Some(1));
        assert_eq!(s.port, 9000);
    }

    #[test]
    fn out_of_range_port_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("launcher.json");
        std::fs::write(&p, r#"{"port":80}"#).unwrap();
        assert_eq!(load_settings(&p).port, DEFAULT_PORT);
        for bad in ["70000", "-5", "\"x\""] {
            std::fs::write(
                &p,
                format!(r#"{{"acceptedTermsVersion":1,"autostart":true,"importOffered":true,"port":{bad}}}"#),
            )
            .unwrap();
            let s = load_settings(&p);
            assert_eq!(s.port, DEFAULT_PORT);
            assert_eq!(s.accepted_terms_version, Some(1));
            assert!(s.autostart && s.import_offered);
        }
    }

    #[test]
    fn an_old_file_without_check_updates_checks_and_keeps_its_consent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("launcher.json");
        std::fs::write(
            &p,
            r#"{"acceptedTermsVersion":1,"port":8787,"autostart":false,"importOffered":true}"#,
        )
        .unwrap();
        let s = load_settings(&p);
        assert!(s.check_updates);
        assert_eq!(s.accepted_terms_version, Some(1));
        assert!(terms_accepted(&s));
    }

    #[test]
    fn check_updates_off_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("launcher.json");
        std::fs::write(&p, r#"{"acceptedTermsVersion":1,"checkUpdates":false}"#).unwrap();
        assert!(!load_settings(&p).check_updates);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("launcher.json");
        let s = LauncherSettings {
            accepted_terms_version: Some(TERMS_VERSION),
            port: 9001,
            autostart: true,
            import_offered: true,
            check_updates: false,
        };
        save_settings(&p, &s).unwrap();
        assert_eq!(load_settings(&p), s);
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file must be renamed away");
    }

    #[test]
    fn validate_port_cases() {
        let msg = "Port must be a number from 1024 to 65535";
        assert_eq!(validate_port("8787"), Ok(8787));
        assert_eq!(validate_port(" 1024 "), Ok(1024));
        assert_eq!(validate_port("80"), Err(msg.to_string()));
        assert_eq!(validate_port("70000"), Err(msg.to_string()));
        assert_eq!(validate_port("x"), Err(msg.to_string()));
    }
}
