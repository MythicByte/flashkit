#![allow(async_fn_in_trait)]
//! # Cross-platform Block Device Flasher
//!
//! A library for **safe, cross-platform flashing** of disk or block device images, such as SD cards or USB drives.
//!
//! This crate is designed to provide a unified and (as much as possible) safe API for:
//! - Enumerating **physical storage devices** (block/USB drives, SD cards, etc.)
//! - **Unmounting** and **ejecting** devices safely before and after imaging.
//! - Writing raw disk images with optional hash verification.
//! - Async-flavored API for progress monitoring, suited for desktop GUIs or CLI progress bars.
//!
//! ## Features
//!
//! - **Cross Platform**: Works on Linux, Windows, and macOS.
//! - **writes**: All writes are page/sector aligned. APIs perform full device unmount/eject.
//! - **Async**: Designed with [tokio] for progress events and scalable UI integration.
//! - **Plug and Play**: Start flashing with minimal platform-specific code.
//!
//! ## Supported Platforms
//!
//! - **Linux**: Uses D-Bus (`UDisks2`) to enumerate, open, unmount, and eject drives in a user-friendly and permission-aware manner.
//! - **Windows**: Interacts with disk devices via Win32 APIs. Handles device unmount, mount-point removal, and raw imaging with exclusive locks.
//! - **macOS**: Interfaces with the `diskutil` and `authopen` utilities to list disks, acquire secure access, and unmount/eject using native system commands.
//!
//! ## Example
//!
//! List block devices and (pseudo-)flash an image (example; real flashing requires a real device and admin privileges).
//! Requires the "tokio" runtime.
//!
//! ```no_run
//! use your_crate::flash;
//! use your_crate::traits::FlasherGeneric;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Initialize the OS-native flasher (platform is auto-detected)
//!     let flasher = flash().await?;
//!     let devices = flasher.list_devices().await?;
//!
//!     println!("Detected block devices:");
//!     for (idx, dev) in devices.iter().enumerate() {
//!         println!("{}: {} ({} bytes, removable: {})", idx, dev.name(), dev.size_in_bytes(), dev.removable());
//!     }
//!
//!     // For demonstration, just pick the first device, and DO NOT run this for real on a system disk!
//!     // let device = &devices[0];
//!     // let test_image_file = tokio::fs::File::open("some-image.img").await?;
//!     // let img_source = your_crate::data_types::AsyncImageSourceFile::new(test_image_file, /*size*/0, None);
//!     // let (tx, _rx) = tokio::sync::watch::channel(Default::default());
//!     // flasher.flash(img_source, device, tx).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! **Caution**: Writing to block devices will destroy all data on the target! Always prompt the user before proceeding in real applications.
//!
//! ## Platform-Specific Details
//!
//! - **Linux**
//!     - Uses **D-Bus `UDisks2`** for all device management (enumeration, unmounting, ejection, open with O_DIRECT).
//!     - Requires the appropriate permissions to use D-Bus UDisks2 (often part of the `plugdev` group or root).
//!     - Works well with modern Linux desktops or headless servers.
//!
//! - **Windows**
//!     - Interacts directly with the **Win32 API**—using WMI for device enumeration, and DeviceIoControl APIs for locking, unmounting, and ejecting drives.
//!     - Opens physical devices such as `\\.\PhysicalDrive0` with exclusive access.
//!     - Handles drive letter/mount point removal before raw access.
//!
//! - **macOS**
//!     - Leverages built-in **`diskutil`** for discovery, unmount, and ejection.
//!     - Uses **`authopen`** for privileged raw device access (prompts for password if required).
//!     - Automates safe detachment of partitions before flashing.
//!
//! ## See Also
//!
//! - Each platform trait and interface is implemented in its module:
//!     - [`linux::LinuxDBus`]
//!     - [`windows::windows::WindowsInterface`]
//!     - [`macos::macos::DarwinInterface`]
//!   and registered under the `OsFlasher` type for ergonomics.
//!
//! ---
//!
//! ## Modules
//!
//! - [`aligned`] — Page-aligned buffer helper for direct I/O.
//! - [`data_types`] — Core structs representing block devices, progress, partitions, etc.
//! - [`traits`] — Extensible trait system for device enumeration, unmounting, writing, and ejection.
//! - [`flasher`] — Core logic for hashing, writing, and verifying images.
//! - [`error`] — Comprehensive error type wrapping all platform error cases.

/// A aligned vec
pub mod aligned;
/// Libary Data types
pub mod data_types;
/// Errors
pub mod error;
/// the flasher
pub mod flasher;
#[cfg(target_os = "linux")]
/// Linux
pub mod linux;
#[cfg(target_os = "macos")]
/// Macos
pub mod macos;
/// Generic Traits abstraction
pub mod traits;
#[cfg(target_os = "windows")]
/// Windows
pub mod windows;
#[cfg(target_os = "windows")]
use crate::windows::windows::WindowsRawWriteHandle as Interface;

#[cfg(target_os = "linux")]
use crate::linux::linux::LinuxDBus as Interface;

#[cfg(target_os = "macos")]
use crate::macos::macos::DarwinInterface as Interface;

#[allow(unused_imports)]
use crate::{
    error::FlashResult,
    traits::Flasher,
};

/// The platform-native Flasher type
pub type OsFlasher = Flasher<Interface>;

/// Automatically creates a Flasher configured for the current operating system.
///
/// - **Linux**: returns a `Flasher<LinuxDBus>`, using D-Bus for block device enumeration and read/write access.
/// - **Windows**: returns a `Flasher<WindowsInterface>`, using WMI and Win32 device APIs.
/// - **macOS**: returns a `Flasher<DarwinInterface>`, using diskutil and authopen for device access.
///
/// # Errors
///
/// Fails if the underlying platform mechanism cannot be initialized (e.g., no DBus, WMI unavailable, etc.).
#[cfg(target_os = "linux")]
#[must_use = "Use or remove libary"]
pub async fn flash() -> FlashResult<OsFlasher> {
    let device = Interface::new().await?;
    Ok(Flasher::new(device))
}
/// Automatically creates a Flasher configured for the current operating system.
#[cfg(target_os = "windows")]
#[must_use = "Use or remove libary"]
pub async fn flash() -> FlashResult<OsFlasher> {
    let device = Interface::default();
    Ok(Flasher::new(device))
}
/// Automatically creates a Flasher configured for the current operating system.
#[cfg(target_os = "macos")]
#[must_use = "Use or remove libary"]
pub async fn flash() -> FlashResult<OsFlasher> {
    let device = Interface;
    Ok(Flasher::new(device))
}
