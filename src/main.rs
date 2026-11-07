mod integration;
mod qemu;

use integration::Integration;
use qemu::{detect_accelerator, detect_qemu, VmConfig, VmRunner};
use slint::ComponentHandle;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

slint::include_modules!();

fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn parse_ram_mb(val: &str) -> u32 {
    val.split_whitespace()
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(2048)
}

fn parse_cores(val: &str) -> u32 {
    val.split_whitespace()
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(2)
}

fn update_iso_details(ui: &AppWindow, path_str: &str) {
    let path = Path::new(path_str);
    if path.exists() && path.is_file() {
        if let Ok(meta) = fs::metadata(path) {
            let size_str = format_file_size(meta.len());
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("ISO File");
            ui.set_iso_info(format!("Selected: {} ({})", name, size_str).into());
            return;
        }
    }
    if path_str.trim().is_empty() {
        ui.set_iso_info("No ISO selected".into());
    } else {
        ui.set_iso_info("Specified file not found or inaccessible".into());
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // Ensure UTF-8 locale fallback on Linux to avoid xkbcommon compose parsing quirks
    #[cfg(target_os = "linux")]
    if env::var("LC_ALL").is_err() {
        env::set_var("LC_ALL", "C.UTF-8");
    }

    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();

    // Initialize QEMU path
    let qemu_bin = detect_qemu().unwrap_or_else(|| PathBuf::from("qemu-system-x86_64"));
    ui.set_qemu_path(qemu_bin.to_string_lossy().to_string().into());

    // Initialize Accelerator
    let accel = detect_accelerator();
    ui.set_accel_info(accel.description().into());

    // Initialize Integration status
    let is_integrated = Integration::is_installed();
    ui.set_is_integrated(is_integrated);
    ui.set_integration_status(if is_integrated { "Installed".into() } else { "Not Installed".into() });

    // Handle CLI arguments (support `liveiso-runner <iso_path>`)
    if let Some(arg) = env::args().nth(1) {
        let p = PathBuf::from(&arg);
        if p.exists() {
            ui.set_iso_path(p.to_string_lossy().to_string().into());
            update_iso_details(&ui, &p.to_string_lossy());
        }
    }

    let runner = Arc::new(VmRunner::new());

    // Callback: Browse ISO
    {
        let ui_handle = ui_weak.clone();
        ui.on_browse_iso(move || {
            if let Some(file) = rfd::FileDialog::new()
                .set_title("Select Bootable ISO or Disk Image")
                .add_filter("Bootable Images (*.iso, *.img, *.raw, *.qcow2)", &["iso", "img", "raw", "qcow2"])
                .add_filter("All Files", &["*"])
                .pick_file()
            {
                if let Some(ui) = ui_handle.upgrade() {
                    let path_str = file.to_string_lossy().to_string();
                    ui.set_iso_path(path_str.clone().into());
                    update_iso_details(&ui, &path_str);
                }
            }
        });
    }

    // Callback: Browse custom QEMU
    {
        let ui_handle = ui_weak.clone();
        ui.on_browse_qemu(move || {
            if let Some(file) = rfd::FileDialog::new()
                .set_title("Locate QEMU executable")
                .pick_file()
            {
                if let Some(ui) = ui_handle.upgrade() {
                    ui.set_qemu_path(file.to_string_lossy().to_string().into());
                }
            }
        });
    }

    // Callback: Run VM
    {
        let runner_clone = runner.clone();
        let ui_handle = ui_weak.clone();
        ui.on_run_vm(move || {
            let Some(ui) = ui_handle.upgrade() else { return };

            let iso_path_str = ui.get_iso_path().to_string();
            let iso_path = PathBuf::from(&iso_path_str);
            if !iso_path.exists() {
                ui.set_vm_status("Error: File not found".into());
                return;
            }

            let qemu_path = PathBuf::from(ui.get_qemu_path().to_string());
            let ram_mb = parse_ram_mb(&ui.get_selected_ram().to_string());
            let cores = parse_cores(&ui.get_selected_cpu().to_string());
            let enable_network = ui.get_enable_net();
            let accel = detect_accelerator();

            let config = VmConfig {
                qemu_path,
                iso_path,
                ram_mb,
                cores,
                accel,
                enable_network,
            };

            ui.set_is_vm_running(true);
            ui.set_vm_error("".into());
            ui.set_vm_status("Launching QEMU...".into());

            let ui_weak_exit = ui_handle.clone();
            let launch_res = runner_clone.launch(config, move |result| {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_exit.upgrade() {
                        ui.set_is_vm_running(false);
                        ui.set_vm_status(result.summary().into());
                        if !result.is_success() {
                            let details = if result.stderr_tail.is_empty() {
                                "(QEMU printed no error output)".to_string()
                            } else {
                                result.stderr_tail
                            };
                            ui.set_vm_error(details.into());
                        }
                    }
                });
            });

            if let Err(e) = launch_res {
                ui.set_is_vm_running(false);
                ui.set_vm_status("Launch failed".into());
                ui.set_vm_error(e.into());
            } else {
                ui.set_vm_status("VM Active".into());
            }
        });
    }

    // Callback: Stop VM
    {
        let runner_clone = runner.clone();
        let ui_handle = ui_weak.clone();
        ui.on_stop_vm(move || {
            let _ = runner_clone.stop();
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_is_vm_running(false);
                ui.set_vm_status("Stopped by user".into());
            }
        });
    }

    // Callback: Install integration
    {
        let ui_handle = ui_weak.clone();
        ui.on_install_integration(move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            match Integration::install() {
                Ok(_) => {
                    ui.set_is_integrated(true);
                    ui.set_integration_status("Installed successfully".into());
                }
                Err(e) => {
                    ui.set_integration_status(format!("Error: {}", e).into());
                }
            }
        });
    }

    // Callback: Uninstall integration
    {
        let ui_handle = ui_weak.clone();
        ui.on_uninstall_integration(move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            match Integration::uninstall() {
                Ok(_) => {
                    ui.set_is_integrated(false);
                    ui.set_integration_status("Removed".into());
                }
                Err(e) => {
                    ui.set_integration_status(format!("Error: {}", e).into());
                }
            }
        });
    }

    ui.run()
}
