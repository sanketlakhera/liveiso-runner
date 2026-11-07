use std::env;
use std::fs;
use std::path::PathBuf;

#[cfg(windows)]
use winreg::enums::*;
#[cfg(windows)]
use winreg::RegKey;

pub struct Integration;

#[cfg(windows)]
const WIN_KEY: &str = "Software\\Classes\\.iso\\shell\\LiveISORunner";
/// Key written by builds released under the old name; removed on uninstall.
#[cfg(windows)]
const LEGACY_WIN_KEY: &str = "Software\\Classes\\.iso\\shell\\MobaLiveCD";

impl Integration {
    pub fn is_installed() -> bool {
        #[cfg(target_os = "linux")]
        {
            if let Some(desktop_file) = Self::linux_desktop_path() {
                desktop_file.exists()
            } else {
                false
            }
        }

        #[cfg(target_os = "windows")]
        {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            hkcu.open_subkey(WIN_KEY).is_ok()
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            false
        }
    }

    pub fn install() -> Result<(), String> {
        let current_exe = env::current_exe().map_err(|e| format!("Failed to get exe path: {}", e))?;

        #[cfg(target_os = "linux")]
        {
            let desktop_path = Self::linux_desktop_path().ok_or("Cannot determine user applications folder")?;
            if let Some(parent) = desktop_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {}", e))?;
            }

            let content = format!(
                "[Desktop Entry]\n\
                Type=Application\n\
                Name=Test with LiveISO Runner\n\
                GenericName=LiveCD Emulator\n\
                Comment=Test bootable CD-ROM or ISO image with QEMU\n\
                Exec=\"{}\" %f\n\
                Terminal=false\n\
                MimeType=application/x-cd-image;application/x-iso9660-image;application/x-raw-disk-image;\n\
                Categories=System;Emulator;Utility;\n\
                Icon=system-run\n",
                current_exe.to_string_lossy()
            );

            fs::write(&desktop_path, content).map_err(|e| format!("Failed to write desktop file: {}", e))?;

            // Try running update-desktop-database if available
            let _ = std::process::Command::new("update-desktop-database")
                .arg(desktop_path.parent().unwrap())
                .output();

            Ok(())
        }

        #[cfg(target_os = "windows")]
        {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let (key, _) = hkcu.create_subkey(WIN_KEY)
                .map_err(|e| format!("Failed to create registry key: {}", e))?;
            key.set_value("", &"Test with LiveISO Runner")
                .map_err(|e| format!("Failed to set key description: {}", e))?;

            let (cmd_key, _) = key.create_subkey("command")
                .map_err(|e| format!("Failed to create command key: {}", e))?;
            let cmd_val = format!("\"{}\" \"%1\"", current_exe.to_string_lossy());
            cmd_key.set_value("", &cmd_val)
                .map_err(|e| format!("Failed to set command value: {}", e))?;

            Ok(())
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Err("Context menu integration not supported on this OS".to_string())
        }
    }

    pub fn uninstall() -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            // Remove the entry left by builds released under the old name.
            if let Some(legacy) = dirs::data_local_dir().map(|p| p.join("applications/mobalivecd.desktop")) {
                let _ = fs::remove_file(legacy);
            }
            if let Some(desktop_path) = Self::linux_desktop_path() {
                if desktop_path.exists() {
                    fs::remove_file(&desktop_path).map_err(|e| format!("Failed to remove desktop file: {}", e))?;
                    let _ = std::process::Command::new("update-desktop-database")
                        .arg(desktop_path.parent().unwrap())
                        .output();
                }
            }
            Ok(())
        }

        #[cfg(target_os = "windows")]
        {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let _ = hkcu.delete_subkey_all(WIN_KEY);
            let _ = hkcu.delete_subkey_all(LEGACY_WIN_KEY);
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
}
