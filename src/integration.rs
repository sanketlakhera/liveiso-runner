//! Per-user "open with" / context-menu integration for ISO files.
//!
//! Linux: a `.desktop` entry in the user's applications folder.
//! Windows: a verb under `HKCU\Software\Classes\.iso\shell` (no admin rights needed).

use std::io;
use std::path::PathBuf;

#[cfg(windows)]
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

#[cfg(windows)]
const WIN_KEY: &str = "Software\\Classes\\SystemFileAssociations\\.iso\\shell\\LiveISORunner";
/// Keys written by earlier builds (the verb directly under `.iso` is ignored
/// when the extension has its own ProgID); removed on install and uninstall.
#[cfg(windows)]
const LEGACY_WIN_KEYS: [&str; 2] = [
    "Software\\Classes\\.iso\\shell\\LiveISORunner",
    "Software\\Classes\\.iso\\shell\\MobaLiveCD",
];

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[error("cannot determine the user applications folder")]
    NoAppsDir,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[error("executable path cannot be used in a desktop entry (not UTF-8 or contains control characters): {}", .0.display())]
    UnsupportedExePath(PathBuf),
    #[cfg_attr(any(target_os = "linux", target_os = "windows"), allow(dead_code))]
    #[error("file manager integration is not supported on this OS")]
    Unsupported,
}

/// Returns a `map_err` adapter that wraps an I/O error with `context`.
fn io_context(context: &'static str) -> impl FnOnce(io::Error) -> IntegrationError {
    move |source| IntegrationError::Io { context, source }
}

pub fn is_installed() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_desktop_path().is_some_and(|p| p.exists())
    }

    #[cfg(target_os = "windows")]
    {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(WIN_KEY)
            .is_ok()
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

pub fn install() -> Result<(), IntegrationError> {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let current_exe =
        std::env::current_exe().map_err(io_context("failed to get executable path"))?;

    #[cfg(target_os = "linux")]
    {
        let desktop_path = linux_desktop_path().ok_or(IntegrationError::NoAppsDir)?;
        let apps_dir = desktop_path.parent().ok_or(IntegrationError::NoAppsDir)?;
        std::fs::create_dir_all(apps_dir).map_err(io_context("failed to create directory"))?;

        let exec = desktop_exec_quote(
            current_exe
                .to_str()
                .ok_or_else(|| IntegrationError::UnsupportedExePath(current_exe.clone()))?,
        )?;
        let content = format!(
            "[Desktop Entry]\n\
            Type=Application\n\
            Name=Test with LiveISO Runner\n\
            GenericName=LiveCD Emulator\n\
            Comment=Test bootable CD-ROM or ISO image with QEMU\n\
            Exec={exec} %f\n\
            Terminal=false\n\
            MimeType=application/x-cd-image;application/x-iso9660-image;application/x-raw-disk-image;\n\
            Categories=System;Emulator;Utility;\n\
            Icon=system-run\n"
        );
        std::fs::write(&desktop_path, content)
            .map_err(io_context("failed to write desktop file"))?;
        refresh_desktop_database(apps_dir);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        for k in LEGACY_WIN_KEYS {
            let _ = hkcu.delete_subkey_all(k);
        }
        let (key, _) = hkcu
            .create_subkey(WIN_KEY)
            .map_err(io_context("failed to create registry key"))?;
        key.set_value("", &"Test with LiveISO Runner")
            .map_err(io_context("failed to set key description"))?;

        let (cmd_key, _) = key
            .create_subkey("command")
            .map_err(io_context("failed to create command key"))?;
        let cmd_val = format!("\"{}\" \"%1\"", current_exe.to_string_lossy());
        cmd_key
            .set_value("", &cmd_val)
            .map_err(io_context("failed to set command value"))?;
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(IntegrationError::Unsupported)
    }
}

pub fn uninstall() -> Result<(), IntegrationError> {
    #[cfg(target_os = "linux")]
    {
        // Remove the entry left by builds released under the old name.
        if let Some(legacy) =
            dirs::data_local_dir().map(|p| p.join("applications/mobalivecd.desktop"))
        {
            let _ = std::fs::remove_file(legacy);
        }
        if let Some(desktop_path) = linux_desktop_path() {
            match std::fs::remove_file(&desktop_path) {
                Ok(()) => {
                    if let Some(apps_dir) = desktop_path.parent() {
                        refresh_desktop_database(apps_dir);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_context("failed to remove desktop file")(e)),
            }
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hkcu.delete_subkey_all(WIN_KEY);
        for k in LEGACY_WIN_KEYS {
            let _ = hkcu.delete_subkey_all(k);
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn linux_desktop_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|p| p.join("applications/liveiso-runner.desktop"))
}

/// Best effort: refreshes the MIME cache so file managers see the change.
#[cfg(target_os = "linux")]
fn refresh_desktop_database(apps_dir: &std::path::Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(apps_dir)
        .output();
}

/// Quotes `path` as a single argument of a Desktop Entry `Exec` value.
///
/// Per the Desktop Entry Specification, inside a double-quoted argument `"`,
/// `` ` ``, `$` and `\` must be backslash-escaped; because the general string
/// escaping (`\\` for a backslash) is applied first, each of those backslashes
/// is doubled in the file (a literal `\` becomes four backslashes). A literal
/// `%` is written `%%`. Control characters cannot be represented.
#[cfg(any(test, target_os = "linux"))]
fn desktop_exec_quote(path: &str) -> Result<String, IntegrationError> {
    if path.chars().any(char::is_control) {
        return Err(IntegrationError::UnsupportedExePath(PathBuf::from(path)));
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::desktop_exec_quote as q;

    #[test]
    fn plain_and_space() {
        assert_eq!(q("/usr/bin/app").unwrap(), r#""/usr/bin/app""#);
        assert_eq!(q("/my dir/app").unwrap(), r#""/my dir/app""#);
    }

    #[test]
    fn special_characters() {
        assert_eq!(q("/a\"b").unwrap(), r#""/a\\"b""#);
        assert_eq!(q("/a`b").unwrap(), r#""/a\\`b""#);
        assert_eq!(q("/a$b").unwrap(), r#""/a\\$b""#);
        assert_eq!(q("/a\\b").unwrap(), r#""/a\\\\b""#);
        assert_eq!(q("/a%b").unwrap(), r#""/a%%b""#);
    }

    #[test]
    fn combination() {
        assert_eq!(q("/x y/\"$`\\%").unwrap(), r#""/x y/\\"\\$\\`\\\\%%""#);
    }

    #[test]
    fn control_characters_rejected() {
        assert!(q("/a\nb").is_err());
        assert!(q("/a\tb").is_err());
    }
}
