//! Platform data locations; explicit --data-dir is resolved by the caller.
use anyhow::{Context, Result};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy)]
enum Platform {
    Windows,
    Mac,
    Unix,
}

pub(super) fn resolve(portable: bool) -> Result<PathBuf> {
    let platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Mac
    } else {
        Platform::Unix
    };
    if portable {
        return portable_directory(&std::env::current_exe()?, platform);
    }
    user_directory(
        platform,
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("HOME"),
        std::env::var_os("XDG_DATA_HOME"),
    )
}

fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

fn user_directory(
    platform: Platform,
    local: Option<OsString>,
    home: Option<OsString>,
    xdg: Option<OsString>,
) -> Result<PathBuf> {
    let path = match platform {
        Platform::Windows => absolute(local).map(|p| p.join("LXNative")),
        Platform::Mac => absolute(home).map(|p| p.join("Library/Application Support/LXNative")),
        Platform::Unix => absolute(xdg)
            .or_else(|| absolute(home).map(|p| p.join(".local/share")))
            .map(|p| p.join("lx-native")),
    };
    path.context("An absolute user data directory is unavailable; specify --data-dir")
}

fn portable_directory(executable: &Path, platform: Platform) -> Result<PathBuf> {
    let directory = executable
        .parent()
        .context("Executable directory unavailable")?;
    if matches!(platform, Platform::Mac)
        && directory.file_name().is_some_and(|n| n == "MacOS")
        && let Some(contents) = directory.parent()
        && contents.file_name().is_some_and(|n| n == "Contents")
        && let Some(bundle) = contents.parent()
        && bundle.extension().is_some_and(|n| n == "app")
    {
        return Ok(bundle
            .parent()
            .context("Application bundle directory unavailable")?
            .join("userdata"));
    }
    Ok(directory.join("userdata"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> PathBuf {
        std::env::temp_dir().join("qisheng-path-test")
    }
    fn value(path: &Path) -> Option<OsString> {
        Some(path.as_os_str().to_owned())
    }

    #[test]
    fn windows_preserves_existing_profile_location() {
        assert_eq!(
            user_directory(Platform::Windows, value(&base()), None, None).unwrap(),
            base().join("LXNative")
        );
    }
    #[test]
    fn mac_uses_application_support_and_ignores_xdg() {
        assert_eq!(
            user_directory(
                Platform::Mac,
                None,
                value(&base()),
                value(&base().join("xdg"))
            )
            .unwrap(),
            base().join("Library/Application Support/LXNative")
        );
    }
    #[test]
    fn linux_prefers_absolute_xdg() {
        assert_eq!(
            user_directory(
                Platform::Unix,
                None,
                value(&base()),
                value(&base().join("xdg"))
            )
            .unwrap(),
            base().join("xdg/lx-native")
        );
    }
    #[test]
    fn linux_ignores_empty_or_relative_xdg() {
        for path in ["", "relative/data"] {
            assert_eq!(
                user_directory(Platform::Unix, None, value(&base()), Some(path.into())).unwrap(),
                base().join(".local/share/lx-native")
            );
        }
    }
    #[test]
    fn missing_absolute_home_requires_explicit_selection() {
        assert!(user_directory(Platform::Unix, None, Some("relative".into()), None).is_err());
        assert!(user_directory(Platform::Mac, None, None, value(&base())).is_err());
        assert!(user_directory(Platform::Windows, Some("".into()), value(&base()), None).is_err());
    }
    #[test]
    fn mac_portable_data_stays_outside_the_signed_bundle() {
        let executable = base().join("栖声.app/Contents/MacOS/qisheng");
        assert_eq!(
            portable_directory(&executable, Platform::Mac).unwrap(),
            base().join("userdata")
        );
    }
    #[test]
    fn standalone_portable_data_is_adjacent_to_the_executable() {
        for platform in [Platform::Windows, Platform::Mac, Platform::Unix] {
            assert_eq!(
                portable_directory(&base().join("qisheng"), platform).unwrap(),
                base().join("userdata")
            );
        }
    }
}
