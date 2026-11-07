# LiveISO Runner

[![CI](https://github.com/sanketlakhera/liveiso-runner/actions/workflows/ci.yml/badge.svg)](https://github.com/sanketlakhera/liveiso-runner/actions/workflows/ci.yml)

A fast, cross-platform tool for testing bootable live CDs, ISOs and disk images in QEMU, without burning media or setting up a virtual machine by hand.

> Inspired by **MobaLiveCD**, the classic 2008 Windows utility by Mobatek. LiveISO Runner is an independent rewrite and is not affiliated with or endorsed by Mobatek.

Written in **Rust** using **Slint** for native GUI rendering on both **Linux** and **Windows**.

---

## Features

- **Direct LiveCD Execution**: Run any `.iso`, `.img`, `.raw`, or `.qcow2` image without burning media or setting up a complex virtual machine.
- **Hardware Acceleration**:
  - **Linux**: Uses **KVM** when `/dev/kvm` is accessible (`-accel kvm -accel tcg,thread=multi`); otherwise multi-threaded **TCG** only.
  - **Windows**: Requests **WHPX** (Windows Hypervisor Platform) and passes a TCG fallback (`-accel whpx -accel tcg,thread=multi`), so QEMU falls back to TCG if WHPX is not enabled.
- **Modern Hardware Configuration**:
  - Memory selection (512 MB to 8192 MB)
  - Multi-core CPU allocation (1 to 8 cores)
  - Virtual network toggle (User NAT)
- **OS File Association & Context Menu**:
  - **Linux**: One-click install/uninstall for file managers (`.desktop` MIME association for `application/x-cd-image`).
  - **Windows**: Non-admin user context menu integration (`HKCU\Software\Classes\SystemFileAssociations\.iso\shell`).
- **CLI Image Passing**:
  - Pass an ISO directly on the command line:
    ```bash
    cargo run -- /path/to/distro.iso
    ```

---

## Running in Development Mode

### Linux
```bash
cd liveiso-runner
cargo run
```

To test with a specific ISO file immediately:
```bash
cargo run -- /path/to/my-image.iso
```

### Windows
```cmd
cd liveiso-runner
cargo run
```

---

## Building a Release Binary

```bash
cargo build --release
```
The optimized single binary will be located in:
- Linux: `target/release/liveiso-runner`
- Windows: `target/release/liveiso-runner.exe`

QEMU is not bundled. Install it from your package manager (Linux) or from [qemu.org](https://www.qemu.org/download/) (Windows).

---

## License

Copyright (C) 2026 Sanket

LiveISO Runner is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE) for the full text.

The user interface is built with [Slint](https://slint.dev), used under its GPLv3 license.
