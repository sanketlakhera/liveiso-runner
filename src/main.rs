mod integration;
mod qemu;

use qemu::{detect_accelerator, detect_qemu, VmConfig, VmExit, VmRunner};
use slint::ComponentHandle;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

slint::include_modules!();

const DEFAULT_RAM_MB: u32 = 2048;
const DEFAULT_CORES: u32 = 2;

#[allow(clippy::cast_precision_loss)] // display only; two decimals at most
fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    match bytes {
        b if b >= GB => format!("{:.2} GB", b as f64 / GB as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= KB => format!("{:.1} KB", b as f64 / KB as f64),
        b => format!("{b} B"),
    }
}

/// Parses the leading number of a UI label such as "2048 MB" or "4 Cores".
fn parse_leading_number(label: &str, default: u32) -> u32 {
    label
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn update_image_details(ui: &AppWindow, path_str: &str) {
    let path = Path::new(path_str);
    let info = match fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            let name = path
                .file_name()
                .map_or_else(|| "ISO File".into(), |n| n.to_string_lossy());
            format!("Selected: {name} ({})", format_file_size(meta.len()))
        }
        _ if path_str.trim().is_empty() => "No ISO selected".to_owned(),
        _ => "Specified file not found or inaccessible".to_owned(),
    };
    ui.set_iso_info(info.into());
}

fn main() -> Result<(), slint::PlatformError> {
    // Ensure UTF-8 locale fallback on Linux to avoid xkbcommon compose parsing quirks.
    // Runs before any threads are spawned (required once this becomes `unsafe` in edition 2024).
    #[cfg(target_os = "linux")]
    if env::var_os("LC_ALL").is_none() {
        env::set_var("LC_ALL", "C.UTF-8");
    }

    let ui = AppWindow::new()?;
    init_state(&ui);

    let runner = Arc::new(VmRunner::new());
    setup_browse_image(&ui);
    setup_browse_qemu(&ui);
    setup_run_vm(&ui, Arc::clone(&runner));
    setup_stop_vm(&ui, runner);
    setup_integration(&ui);

    ui.run()
}

/// Fills in the detected QEMU path, accelerator, integration status, and an
/// image passed on the command line (`liveiso-runner <image_path>`).
fn init_state(ui: &AppWindow) {
    let qemu_bin = detect_qemu().unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"));
    ui.set_qemu_path(qemu_bin.to_string_lossy().into_owned().into());
    ui.set_accel_info(detect_accelerator().description().into());

    let is_integrated = integration::is_installed();
    ui.set_is_integrated(is_integrated);
    ui.set_integration_status(
        if is_integrated {
            "Installed"
        } else {
            "Not Installed"
        }
        .into(),
    );

    if let Some(arg) = env::args_os().nth(1) {
        let path = PathBuf::from(arg);
        if path.exists() {
            let path_str = path.to_string_lossy();
            ui.set_iso_path(path_str.as_ref().into());
            update_image_details(ui, &path_str);
        }
    }
}

fn setup_browse_image(ui: &AppWindow) {
    let ui_weak = ui.as_weak();
    ui.on_browse_iso(move || {
        let Some(file) = rfd::FileDialog::new()
            .set_title("Select Bootable ISO or Disk Image")
            .add_filter(
                "Bootable Images (*.iso, *.img, *.raw, *.qcow2)",
                &["iso", "img", "raw", "qcow2"],
            )
            .add_filter("All Files", &["*"])
            .pick_file()
        else {
            return;
        };
        if let Some(ui) = ui_weak.upgrade() {
            let path_str = file.to_string_lossy();
            ui.set_iso_path(path_str.as_ref().into());
            update_image_details(&ui, &path_str);
        }
    });
}

fn setup_browse_qemu(ui: &AppWindow) {
    let ui_weak = ui.as_weak();
    ui.on_browse_qemu(move || {
        let Some(file) = rfd::FileDialog::new()
            .set_title("Locate QEMU executable")
            .pick_file()
        else {
            return;
        };
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_qemu_path(file.to_string_lossy().into_owned().into());
        }
    });
}

fn setup_run_vm(ui: &AppWindow, runner: Arc<VmRunner>) {
    let ui_weak = ui.as_weak();
    ui.on_run_vm(move || {
        let Some(ui) = ui_weak.upgrade() else { return };

        let image_path = PathBuf::from(ui.get_iso_path().as_str());
        if !image_path.exists() {
            ui.set_vm_status("Error: File not found".into());
            return;
        }

        let config = VmConfig {
            qemu_path: PathBuf::from(ui.get_qemu_path().as_str()),
            image_path,
            ram_mb: parse_leading_number(&ui.get_selected_ram(), DEFAULT_RAM_MB),
            cores: parse_leading_number(&ui.get_selected_cpu(), DEFAULT_CORES),
            accel: detect_accelerator(),
            enable_network: ui.get_enable_net(),
        };

        ui.set_is_vm_running(true);
        ui.set_vm_error("".into());
        ui.set_vm_status("Launching QEMU...".into());

        let ui_weak_exit = ui_weak.clone();
        let launch_result = runner.launch(&config, move |exit| {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak_exit.upgrade() {
                    show_vm_exit(&ui, exit);
                }
            });
        });

        match launch_result {
            Ok(()) => ui.set_vm_status("VM Active".into()),
            Err(e) => {
                ui.set_is_vm_running(false);
                ui.set_vm_status("Launch failed".into());
                ui.set_vm_error(e.to_string().into());
            }
        }
    });
}

fn show_vm_exit(ui: &AppWindow, exit: VmExit) {
    ui.set_is_vm_running(false);
    ui.set_vm_status(exit.summary().into());
    if !exit.is_success() {
        let details = if exit.stderr_tail.is_empty() {
            "(QEMU printed no error output)".to_owned()
        } else {
            exit.stderr_tail
        };
        ui.set_vm_error(details.into());
    }
}

fn setup_stop_vm(ui: &AppWindow, runner: Arc<VmRunner>) {
    let ui_weak = ui.as_weak();
    ui.on_stop_vm(move || {
        // NotRunning just means QEMU already exited; the UI is reset either way.
        let _ = runner.stop();
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_is_vm_running(false);
            ui.set_vm_status("Stopped by user".into());
        }
    });
}

fn setup_integration(ui: &AppWindow) {
    let ui_weak = ui.as_weak();
    ui.on_install_integration(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match integration::install() {
            Ok(()) => {
                ui.set_is_integrated(true);
                ui.set_integration_status("Installed successfully".into());
            }
            Err(e) => ui.set_integration_status(format!("Error: {e}").into()),
        }
    });

    let ui_weak = ui.as_weak();
    ui.on_uninstall_integration(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match integration::uninstall() {
            Ok(()) => {
                ui.set_is_integrated(false);
                ui.set_integration_status("Removed".into());
            }
            Err(e) => ui.set_integration_status(format!("Error: {e}").into()),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_leading_number_or_default() {
        assert_eq!(parse_leading_number("4096 MB", 1), 4096);
        assert_eq!(parse_leading_number("4 Cores", 1), 4);
        assert_eq!(parse_leading_number("", 7), 7);
        assert_eq!(parse_leading_number("lots", 7), 7);
    }

    #[test]
    fn formats_file_sizes() {
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1536), "1.5 KB");
        assert_eq!(format_file_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(format_file_size(5 * 1024 * 1024 * 1024 / 2), "2.50 GB");
    }
}
