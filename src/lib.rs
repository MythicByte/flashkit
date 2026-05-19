//! A libary for flashing data to a storage device / block device
//!
//! **Cross Platform Support**
//!
//! # Supported
//! - [x] Linux
//! - [ ] Windows
//! - [ ] Macos
//!

/// Libary Data types
pub mod data_types;
/// Errors
pub mod error;
// #[cfg(target_os = "linux")]
// /// Linux
// pub mod linux;
#[cfg(target_os = "linux")]
/// Linux
pub mod linux2;
#[cfg(target_os = "macos")]
/// Macos
pub mod macos;
/// Generic Traits abstraction
pub mod traits;
#[cfg(target_os = "windows")]
/// Windows
pub mod windows;

#[cfg(target_os = "linux")]
use linux2::{
    LinuxDBus as SysEjector,
    LinuxDBus as SysEnumerator,
    LinuxDBus as SysUnmounter,
    LinuxDBus as SysWriter,
};

use crate::{
    error::FlashResult,
    traits::Flasher,
};

// #[cfg(target_os = "windows")]
// use windows::{
//     WindowsDeviceEjector as SysEjector,
//     WindowsDeviceEnumerator as SysEnumerator,
//     WindowsDeviceUnmounter as SysUnmounter,
//     WindowsDeviceWriter as SysWriter,
// };

// #[cfg(target_os = "macos")]
// use macos::{
//     MacosDeviceEjector as SysEjector,
//     MacosDeviceEnumerator as SysEnumerator,
//     MacosDeviceUnmounter as SysUnmounter,
//     MacosDeviceWriter as SysWriter,
// };

/// The platform-native Flasher type
pub type OsFlasher = Flasher<SysEnumerator, SysUnmounter, SysWriter, SysEjector>;

/// Automatically creates a Flasher configured for the current operating system.
#[must_use]
pub async fn flash() -> FlashResult<OsFlasher> {
    let device = SysEnumerator::new().await?;
    Ok(Flasher::new(
        device.clone(),
        device.clone(),
        device.clone(),
        device.clone(),
        1024 * 1024, // 1MB default chunk size
    ))
}
