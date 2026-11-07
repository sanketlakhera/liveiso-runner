use std::ffi::OsString;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use which::which;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Accelerator {
    Kvm,
    Whpx,
    Tcg,
}

impl Accelerator {
    pub fn description(&self) -> &'static str {
        match self {
            Accelerator::Kvm => "KVM (falls back to TCG)",
            Accelerator::Whpx => "WHPX (falls back to TCG)",
            Accelerator::Tcg => "Software TCG",
        }
    }

    /// Values for repeated `-accel` options, in priority order. QEMU uses the
    /// first accelerator that initializes, so hardware ones are followed by TCG.
    pub fn accel_values(&self) -> &'static [&'static str] {
        match self {
            Accelerator::Kvm => &["kvm", "tcg,thread=multi"],
            Accelerator::Whpx => &["whpx", "tcg,thread=multi"],
            Accelerator::Tcg => &["tcg,thread=multi"],
        }
    }
}

pub fn detect_accelerator() -> Accelerator {
    #[cfg(target_os = "linux")]
    {
        if Path::new("/dev/kvm").exists() {
            if File::options().read(true).write(true).open("/dev/kvm").is_ok() {
                return Accelerator::Kvm;
            }
        }
        Accelerator::Tcg
    }

    #[cfg(target_os = "windows")]
    {
        Accelerator::Whpx
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Accelerator::Tcg
    }
}

pub fn detect_qemu() -> Option<PathBuf> {
    // Check user local bin first (common on Linux)
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local/bin/qemu-system-x86_64");
        if local_bin.exists() {
            return Some(local_bin);
        }
    }

    // Check system PATH
    let binary_name = if cfg!(windows) {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };

    if let Ok(path) = which(binary_name) {
        return Some(path);
    }

    // Check standard Windows install paths
    #[cfg(windows)]
    {
        let std_paths = [
            "C:\\Program Files\\qemu\\qemu-system-x86_64.exe",
            "C:\\Program Files (x86)\\qemu\\qemu-system-x86_64.exe",
        ];
        for p in std_paths {
            let path = PathBuf::from(p);
            if path.exists() {
                return Some(path);
            }
        }
    }

    None
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub qemu_path: PathBuf,
    pub iso_path: PathBuf,
    pub ram_mb: u32,
    pub cores: u32,
    pub accel: Accelerator,
    pub enable_network: bool,
}

/// Builds the QEMU command-line arguments (excluding the program path).
///
/// Disk images are passed via `-drive` whose value is a comma-separated option
/// string, so the path is escaped (`,` -> `,,`) and must be valid UTF-8; an
/// explicit `format=` is always given so QEMU never probes the image format.
/// `-cdrom <file>` takes the path as one raw argv entry (verified against
/// QEMU 11: a comma in an ISO path is NOT parsed), so ISOs stay lossless.
pub fn build_args(config: &VmConfig) -> Result<Vec<OsString>, String> {
    let mut args: Vec<OsString> = Vec::new();

    let ext = config.iso_path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if ext == "iso" {
        args.push("-cdrom".into());
        args.push(config.iso_path.clone().into_os_string());
        args.push("-boot".into());
        args.push("d".into());
    } else {
        // Raw disk image / USB image (.img, .raw, .qcow2, ...)
        let path = config.iso_path.to_str().ok_or_else(|| {
            format!(
                "Disk image path is not valid UTF-8 and cannot be passed to QEMU safely: {}",
                config.iso_path.display()
            )
        })?;
        args.push("-drive".into());
        args.push(
            format!("file={},format={},if=ide,media=disk", path.replace(',', ",,"), drive_format(&ext))
                .into(),
        );
        args.push("-boot".into());
        args.push("c".into());
    }

    args.push("-m".into());
    args.push(format!("{}M", config.ram_mb).into());
    args.push("-smp".into());
    args.push(format!("cores={}", config.cores).into());
    args.push("-vga".into());
    args.push("std".into());

    for v in config.accel.accel_values() {
        args.push("-accel".into());
        args.push((*v).into());
    }

    if config.enable_network {
        args.extend(["-net", "nic", "-net", "user"].map(OsString::from));
    } else {
        args.extend(["-net", "none"].map(OsString::from));
    }

    Ok(args)
}

/// Maps a (lowercase) file extension to an explicit QEMU block format name.
fn drive_format(ext: &str) -> &'static str {
    match ext {
        "qcow2" => "qcow2",
        "vmdk" => "vmdk",
        "vdi" => "vdi",
        "vhd" => "vpc",
        "vhdx" => "vhdx",
        _ => "raw",
    }
}

/// Maximum number of stderr lines kept for the UI.
const TAIL_MAX_LINES: usize = 20;
/// Maximum number of stderr bytes kept for the UI.
const TAIL_MAX_BYTES: usize = 4096;
/// Longest chunk read as a single "line" (guards against newline-free output).
const MAX_LINE_READ: u64 = 16 * 1024;
/// How long the waiter waits for the stderr reader to hit EOF after exit.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

/// How a VM process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    /// Exit code 0.
    Success,
    /// Non-zero exit code.
    Failed(i32),
    /// Killed by a signal (no exit code).
    Signaled,
    /// Waiting on the process failed.
    WaitError(String),
}

/// Result handed to the `on_exit` callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmExit {
    pub kind: ExitKind,
    /// Sanitized tail of QEMU's stderr (bounded; may be empty).
    pub stderr_tail: String,
}

impl VmExit {
    pub fn is_success(&self) -> bool {
        self.kind == ExitKind::Success
    }

    /// One-line human summary, e.g. "QEMU failed (exit code 1)".
    pub fn summary(&self) -> String {
        match &self.kind {
            ExitKind::Success => "Exited (code 0)".to_string(),
            ExitKind::Failed(code) => format!("QEMU failed (exit code {})", code),
            ExitKind::Signaled => "QEMU terminated by signal".to_string(),
            ExitKind::WaitError(e) => format!("Error: process error: {}", e),
        }
    }
}

/// Bounded buffer of the most recent sanitized stderr lines.
#[derive(Default)]
struct StderrTail {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    fn push(&mut self, mut line: String) {
        if line.is_empty() {
            return;
        }
        if line.len() > TAIL_MAX_BYTES {
            let mut cut = line.len() - TAIL_MAX_BYTES;
            while !line.is_char_boundary(cut) {
                cut += 1;
            }
            line.drain(..cut);
        }
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.lines.len() > TAIL_MAX_LINES
            || (self.bytes > TAIL_MAX_BYTES && self.lines.len() > 1)
        {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len() + 1;
            }
        }
    }

    fn text(&self) -> String {
        self.lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
    }
}

/// Removes ANSI escape sequences (CSI, OSC, other ESC pairs) and control
/// characters (except tab) from `s`.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // CSI: parameters/intermediates until a final byte 0x40..=0x7e.
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: until BEL or ST (ESC \).
                    while let Some(n) = chars.next() {
                        if n == '\x07' {
                            break;
                        }
                        if n == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Drains `stderr` on its own thread so the pipe never fills and blocks QEMU.
/// Every line is echoed to our own stderr; a bounded sanitized tail is kept.
/// Sends on `done` once EOF is reached.
fn spawn_stderr_reader<R: Read + Send + 'static>(
    stderr: R,
    tail: Arc<Mutex<StderrTail>>,
    done: mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.by_ref().take(MAX_LINE_READ).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let raw = String::from_utf8_lossy(&buf);
                    let raw = raw.trim_end_matches(['\n', '\r']);
                    eprintln!("{}", raw);
                    let clean = sanitize(raw);
                    tail.lock().unwrap_or_else(|e| e.into_inner()).push(clean);
                }
            }
        }
        let _ = done.send(());
    });
}

pub struct VmRunner {
    active_process: Arc<Mutex<Option<Child>>>,
}

impl VmRunner {
    pub fn new() -> Self {
        Self {
            active_process: Arc::new(Mutex::new(None)),
        }
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.active_process.lock().unwrap().is_some()
    }

    /// Kills the running VM and clears the slot. The waiter thread notices the
    /// empty slot and exits without calling `on_exit`, so the caller is
    /// responsible for reporting the user-initiated stop.
    pub fn stop(&self) -> Result<(), String> {
        let child = self.active_process.lock().unwrap().take();
        match child {
            Some(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                Ok(())
            }
            None => Err("No VM is currently running".to_string()),
        }
    }

    pub fn launch<F>(&self, config: VmConfig, on_exit: F) -> Result<(), String>
    where
        F: FnOnce(VmExit) + Send + 'static,
    {
        let mut lock = self.active_process.lock().unwrap();
        if lock.is_some() {
            return Err("A VM is already running".to_string());
        }

        let mut cmd = Command::new(&config.qemu_path);
        cmd.args(build_args(&config)?);

        cmd.stdout(Stdio::inherit())
           .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn QEMU: {}", e))?;
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        let (done_tx, done_rx) = mpsc::channel();
        match child.stderr.take() {
            Some(stderr) => spawn_stderr_reader(stderr, tail.clone(), done_tx),
            None => drop(done_tx),
        }
        let pid = child.id();
        let proc_handle = self.active_process.clone();
        *lock = Some(child);
        drop(lock);

        std::thread::spawn(move || {
            let result = loop {
                {
                    let mut guard = proc_handle.lock().unwrap();
                    match guard.as_mut() {
                        // Slot cleared (or reused by another VM): stopped by user.
                        Some(c) if c.id() == pid => match c.try_wait() {
                            Ok(Some(status)) => {
                                guard.take();
                                break match status.code() {
                                    Some(0) => ExitKind::Success,
                                    Some(code) => ExitKind::Failed(code),
                                    None => ExitKind::Signaled,
                                };
                            }
                            Ok(None) => {}
                            Err(e) => {
                                guard.take();
                                break ExitKind::WaitError(e.to_string());
                            }
                        },
                        _ => return,
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            // Let the reader reach EOF so the tail is complete, but never hang
            // if the pipe is held open (e.g. by an orphaned grandchild).
            let _ = done_rx.recv_timeout(READER_JOIN_TIMEOUT);
            let stderr_tail = tail.lock().unwrap_or_else(|e| e.into_inner()).text();
            on_exit(VmExit { kind: result, stderr_tail });
        });

        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    // Writing an executable while another test thread forks can make exec fail
    // with ETXTBSY (the child inherits the open write fd). Serialize
    // script creation and spawning across tests.
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());

    fn launch_script<F>(runner: &VmRunner, name: &str, body: &str, on_exit: F) -> Result<(), String>
    where
        F: FnOnce(VmExit) + Send + 'static,
    {
        let _g = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        runner.launch(cfg(script(name, body)), on_exit)
    }

    fn script(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("liveiso-runner-test-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("fake-qemu.sh");
        std::fs::write(&p, format!("#!/bin/sh\n{}\n", body)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn cfg(qemu_path: PathBuf) -> VmConfig {
        VmConfig {
            qemu_path,
            iso_path: PathBuf::from("x.iso"),
            ram_mb: 64,
            cores: 1,
            accel: Accelerator::Tcg,
            enable_network: false,
        }
    }

    #[test]
    fn launch_stop_lifecycle() {
        let runner = VmRunner::new();
        let (tx, rx) = mpsc::channel();
        launch_script(&runner, "stop", "exec sleep 30", move |r| tx.send(r).unwrap()).unwrap();
        assert!(runner.is_running());
        assert!(runner.launch(cfg(PathBuf::from("/bin/true")), |_| {}).is_err());
        runner.stop().unwrap();
        assert!(!runner.is_running());
        assert!(runner.stop().is_err());
        // Stopped by user: on_exit is not called (sender dropped by the waiter).
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
    }

    #[test]
    fn natural_exit_calls_on_exit_once() {
        let runner = VmRunner::new();
        let (tx, rx) = mpsc::channel();
        launch_script(&runner, "exit", "exit 3", move |r| tx.send(r).unwrap()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().kind, ExitKind::Failed(3));
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        assert!(!runner.is_running());
    }

    #[test]
    fn signal_death_is_error() {
        let runner = VmRunner::new();
        let (tx, rx) = mpsc::channel();
        launch_script(&runner, "sig", "kill -9 $$", move |r| tx.send(r).unwrap()).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().kind, ExitKind::Signaled);
    }

    fn run_script(name: &str, body: &str) -> VmExit {
        let runner = VmRunner::new();
        let (tx, rx) = mpsc::channel();
        launch_script(&runner, name, body, move |r| tx.send(r).unwrap()).unwrap();
        rx.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    #[test]
    fn stderr_is_captured_on_failure() {
        let exit = run_script("boom", "echo 'boom: something failed' >&2; exit 1");
        assert_eq!(exit.kind, ExitKind::Failed(1));
        assert!(!exit.is_success());
        assert!(exit.stderr_tail.contains("boom: something failed"), "{:?}", exit.stderr_tail);
        assert_eq!(exit.summary(), "QEMU failed (exit code 1)");
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let exit = run_script("many", "i=1; while [ $i -le 1000 ]; do echo \"line $i\" >&2; i=$((i+1)); done; exit 2");
        assert_eq!(exit.kind, ExitKind::Failed(2));
        assert!(exit.stderr_tail.contains("line 1000"));
        assert!(!exit.stderr_tail.lines().any(|l| l == "line 1"));
        assert!(exit.stderr_tail.lines().count() <= TAIL_MAX_LINES);
        assert!(exit.stderr_tail.len() <= TAIL_MAX_BYTES);
    }

    #[test]
    fn success_has_empty_tail() {
        let exit = run_script("ok", "exit 0");
        assert_eq!(exit, VmExit { kind: ExitKind::Success, stderr_tail: String::new() });
        assert!(exit.is_success());
    }

    #[test]
    fn stderr_is_sanitized() {
        let exit = run_script("ansi", "printf '\\033[31mred\\033[0m \\033]0;title\\007ok\\r\\n' >&2; exit 1");
        assert_eq!(exit.stderr_tail, "red ok");
    }

    #[test]
    fn open_pipe_does_not_hang_on_exit() {
        // A background child keeps stderr open after the main process exits.
        let start = std::time::Instant::now();
        let exit = run_script("held", "echo early >&2; (sleep 5 >/dev/null) & exit 1");
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(exit.kind, ExitKind::Failed(1));
        assert!(exit.stderr_tail.contains("early"));
    }

    /// Real-QEMU check; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_qemu_missing_image_reports_error() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let qemu = PathBuf::from(home).join(".local/bin/qemu-system-x86_64");
        if !qemu.exists() {
            eprintln!("skipping: {} not found", qemu.display());
            return;
        }
        let runner = VmRunner::new();
        let (tx, rx) = mpsc::channel();
        let mut c = cfg(qemu);
        c.iso_path = PathBuf::from("/nonexistent/definitely-missing.iso");
        runner.launch(c, move |r| tx.send(r).unwrap()).unwrap();
        let exit = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        eprintln!("exit: {:?}", exit);
        assert!(matches!(exit.kind, ExitKind::Failed(_)));
        assert!(exit.stderr_tail.contains("Could not open"), "{:?}", exit.stderr_tail);
    }
}

#[cfg(test)]
mod args_tests {
    use super::*;

    fn config(iso: &str, accel: Accelerator) -> VmConfig {
        VmConfig {
            qemu_path: PathBuf::from("qemu"),
            iso_path: PathBuf::from(iso),
            ram_mb: 512,
            cores: 2,
            accel,
            enable_network: false,
        }
    }

    fn accel_seq(c: &VmConfig) -> Vec<String> {
        let args: Vec<String> = build_args(c).unwrap().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let mut out = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if a == "-accel" {
                out.push(args[i + 1].clone());
            }
        }
        out
    }

    #[test]
    fn kvm_accel_sequence() {
        assert_eq!(accel_seq(&config("a.iso", Accelerator::Kvm)), ["kvm", "tcg,thread=multi"]);
    }

    #[test]
    fn whpx_accel_sequence() {
        assert_eq!(accel_seq(&config("a.iso", Accelerator::Whpx)), ["whpx", "tcg,thread=multi"]);
    }

    #[test]
    fn tcg_accel_sequence() {
        assert_eq!(accel_seq(&config("a.iso", Accelerator::Tcg)), ["tcg,thread=multi"]);
    }

    #[test]
    fn iso_uses_cdrom_and_boot_d() {
        let args = build_args(&config("a.ISO", Accelerator::Tcg)).unwrap();
        assert_eq!(&args[..4], ["-cdrom", "a.ISO", "-boot", "d"].map(OsString::from));
    }

    #[test]
    fn img_uses_drive_and_boot_c() {
        let args = build_args(&config("a.img", Accelerator::Tcg)).unwrap();
        assert_eq!(
            &args[..4],
            ["-drive", "file=a.img,format=raw,if=ide,media=disk", "-boot", "c"].map(OsString::from)
        );
    }

    fn drive_value(path: &str) -> OsString {
        build_args(&config(path, Accelerator::Tcg)).unwrap()[1].clone()
    }

    #[test]
    fn drive_path_commas_are_doubled() {
        assert_eq!(drive_value("a,b.img"), OsString::from("file=a,,b.img,format=raw,if=ide,media=disk"));
        assert_eq!(
            drive_value("/tmp/a,format=qcow2.img"),
            OsString::from("file=/tmp/a,,format=qcow2.img,format=raw,if=ide,media=disk")
        );
    }

    #[test]
    fn drive_format_mapping() {
        for (name, fmt) in [
            ("a.qcow2", "qcow2"), ("a.vmdk", "vmdk"), ("a.vdi", "vdi"), ("a.vhd", "vpc"),
            ("a.vhdx", "vhdx"), ("a.IMG", "raw"), ("a.QCOW2", "qcow2"), ("a.raw", "raw"),
            ("a.xyz", "raw"), ("noext", "raw"),
        ] {
            assert_eq!(
                drive_value(name),
                OsString::from(format!("file={name},format={fmt},if=ide,media=disk")),
                "{name}"
            );
        }
    }

    #[test]
    fn iso_path_is_passed_verbatim_even_with_commas() {
        let args = build_args(&config("/x/my,disk.iso", Accelerator::Tcg)).unwrap();
        assert_eq!(&args[..4], ["-cdrom", "/x/my,disk.iso", "-boot", "d"].map(OsString::from));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_disk_path_is_error_but_iso_is_lossless() {
        use std::os::unix::ffi::OsStringExt;
        let bad = |name: &str| {
            let mut v = b"/x/\xff".to_vec();
            v.extend_from_slice(name.as_bytes());
            let mut c = config("a.img", Accelerator::Tcg);
            c.iso_path = PathBuf::from(OsString::from_vec(v));
            c
        };
        let err = build_args(&bad(".img")).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
        let iso = bad(".iso");
        let args = build_args(&iso).unwrap();
        assert_eq!(args[1], iso.iso_path.clone().into_os_string());
    }
}
