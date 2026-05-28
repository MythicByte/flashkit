# flashkit

**flashkit** is a cross-platform Rust library for **safe, robust flashing** of raw disk images to USB drives, SD cards, and other block devices—backed by appropriate device enumeration, unmounting, and ejection support. It is designed to power graphical or CLI flashing tools and provide a unified abstraction across Windows, Linux, and macOS.

---

## Purpose

`flashkit` enables you to build reliable, user-friendly disk image writers. It:

- Detects and lists block devices (e.g., USB sticks, SD cards, external HDDs).
- Ensures all filesystems are cleanly unmounted before flashing.
- Writes images with page/sector-aligned direct I/O for maximum safety.
- Optionally checks SHA256 hashes pre/post flashing for data integrity.
- Sends progress events, suitable for both CLI progress bars and GUI UIs.
- Ejects devices after flashing, minimizing post-flash user confusion or filesystem corruption.

**Use cases:**  
- AppImage/ISO/IMG writing utilities (like Etcher or Raspberry Pi Imager)  
- Embedded firmware update tools  
- Automated test infrastructure for disk image deployment

---

## Supported Platforms & Requirements

### Linux

- Uses **DBus UDisks2** via system bus for device detection, management, and raw disk access.
- Requires `dbus` access (`plugdev` group or root for most desktops/servers).
- No additional user intervention; privilege elevation (sudo) may be needed for raw writes.
- Tested with most modern Linux distributions.

### Windows

- Communicates with disks via **Win32 APIs** (WMI, DeviceIoControl etc).
- Requires Administrator privileges to open raw devices and manipulate mount points.
- Ejects and unmounts using FSCTL_* IOCTLs.
-  Works on both desktop and server versions of Windows 10/11.

### macOS

- Uses **diskutil** and **authopen** for device enumeration and raw write access.
- Users may be prompted for password (via `authopen`) to access physical devices.
- Handles ejection via `diskutil eject`.
- Supports both Intel and Apple Silicon Macs.

---

## Getting Started

Add this to your `Cargo.toml`:

```toml
flashkit = { git = "https://gitlab.com/MythicByte/flashkit" }
```

### Minimal Example

```rust
use flashkit::flash;
use flashkit::traits::FlasherGeneric;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flasher = flash().await?;
    let devices = flasher.list_devices().await?;

    println!("Disks detected:");
    for device in devices {
        println!("{:?}", device);
    }

    // To flash: choose device, open image, then call `flasher.flash(...)`
    // See crate documentation for usage pattern.
    Ok(())
}
```

---

## Platform Specific Notes

### Linux
- Needs D-Bus running (all desktops provide this).
- Sufficient permissions are required; often, being in the `plugdev` or `disk` group is enough, otherwise run as root.
- Handles all unmounting/eject stages automatically.

### Windows
- Must run as an Administrator.
- All partitions on the device to be flashed will be unmounted. Take care **not to select your system disk**.

### macOS
- May require the user to approve device access with their password in a GUI dialog.
- Automated unmount and eject using system tools.

---

## Safety Warnings

- **Flashing a drive will destroy all data on the target.** Always explicitly ask users to confirm their target.
- Try to avoid flashing your system drive!

---

## License

Dual-licensed under either:

- [MIT](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

You may use this project under either license.

---

_Mirror of [https://gitlab.com/MythicByte/flashkit](https://gitlab.com/MythicByte/flashkit)_
