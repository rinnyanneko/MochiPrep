//! When the launcher looks for a new release, how it labels one, and in which order it installs
//! it. Pure functions so the rules are testable without a window or a network.

use std::ffi::OsStr;
use std::time::{Duration, Instant};

/// The release's update manifest. GitHub's `latest` redirect always points at the newest
/// published release, so this one address never needs to change.
pub const UPDATE_ENDPOINT: &str =
    "https://github.com/rinnyanneko/MochiPrep/releases/latest/download/latest.json";
/// Where an install that cannot replace itself (a system package) sends the owner instead.
pub const RELEASES_PAGE: &str = "https://github.com/rinnyanneko/MochiPrep/releases/latest";

pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// The updater can replace the installed files itself.
    SelfUpdating,
    /// A system package manager owns the files, so the launcher only says a version exists.
    PackageManaged,
}

/// Windows and macOS installs, and a Linux AppImage, update themselves. The AppImage runtime sets
/// `APPIMAGE` to the image's path; any other Linux install (a `.deb`, a distribution package)
/// belongs to the package manager, and writing over its files would break it.
pub fn install_kind(os: &str, appimage: Option<&OsStr>) -> InstallKind {
    let is_appimage = appimage.is_some_and(|path| !path.is_empty());
    if os == "linux" && !is_appimage {
        InstallKind::PackageManaged
    } else {
        InstallKind::SelfUpdating
    }
}

/// A newer release the last check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer {
    pub version: String,
    pub kind: InstallKind,
}

pub fn update_label(offer: &UpdateOffer) -> String {
    match offer.kind {
        InstallKind::SelfUpdating => format!("Update to v{}", offer.version),
        InstallKind::PackageManaged => format!("v{} available", offer.version),
    }
}

/// A failed check counts as an attempt, so an unreachable GitHub is retried a day later rather
/// than hammered.
pub fn check_due(enabled: bool, last_attempt: Option<Instant>, now: Instant) -> bool {
    if !enabled {
        return false;
    }
    match last_attempt {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= CHECK_INTERVAL,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStep {
    Download,
    StopBackend,
    Install,
    Relaunch,
}

/// The download comes first, while the backend still runs: a slow or failed download then costs
/// nothing, and the backend is down only for the install itself. If the install fails, the
/// launcher starts the backend again.
pub fn update_steps() -> [UpdateStep; 4] {
    [
        UpdateStep::Download,
        UpdateStep::StopBackend,
        UpdateStep::Install,
        UpdateStep::Relaunch,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_without_an_appimage_is_package_managed() {
        assert_eq!(install_kind("linux", None), InstallKind::PackageManaged);
        assert_eq!(
            install_kind("linux", Some(OsStr::new(""))),
            InstallKind::PackageManaged
        );
    }

    #[test]
    fn an_appimage_windows_and_macos_update_themselves() {
        assert_eq!(
            install_kind("linux", Some(OsStr::new("/tmp/MochiPrep.AppImage"))),
            InstallKind::SelfUpdating
        );
        assert_eq!(install_kind("windows", None), InstallKind::SelfUpdating);
        assert_eq!(install_kind("macos", None), InstallKind::SelfUpdating);
    }

    #[test]
    fn labels_name_the_version() {
        let offer = |kind| UpdateOffer {
            version: "0.2.0".to_string(),
            kind,
        };
        assert_eq!(
            update_label(&offer(InstallKind::SelfUpdating)),
            "Update to v0.2.0"
        );
        assert_eq!(
            update_label(&offer(InstallKind::PackageManaged)),
            "v0.2.0 available"
        );
    }

    #[test]
    fn a_check_is_due_once_a_day() {
        let start = Instant::now();
        assert!(check_due(true, None, start));
        assert!(!check_due(
            true,
            Some(start),
            start + Duration::from_secs(23 * 3600)
        ));
        assert!(check_due(true, Some(start), start + CHECK_INTERVAL));
    }

    #[test]
    fn no_check_when_disabled() {
        let start = Instant::now();
        assert!(!check_due(false, None, start));
        assert!(!check_due(false, Some(start), start + CHECK_INTERVAL * 2));
    }

    #[test]
    fn the_download_comes_before_the_backend_stops() {
        assert_eq!(
            update_steps(),
            [
                UpdateStep::Download,
                UpdateStep::StopBackend,
                UpdateStep::Install,
                UpdateStep::Relaunch
            ]
        );
    }

    #[test]
    fn the_endpoint_and_release_page_are_the_projects() {
        assert_eq!(
            UPDATE_ENDPOINT,
            "https://github.com/rinnyanneko/MochiPrep/releases/latest/download/latest.json"
        );
        assert_eq!(
            RELEASES_PAGE,
            "https://github.com/rinnyanneko/MochiPrep/releases/latest"
        );
    }
}
