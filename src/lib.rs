#![allow(async_fn_in_trait)]
//! A libary for flashing data to a storage device / block device
//!
//! **Cross Platform Support**
//!
//! # Supported
//! - [x] Linux
//! - [ ] Windows
//! - [ ] Macos
//!

/// A aligned vec
#[cfg(unix)]
pub mod aligned;
/// Libary Data types
pub mod data_types;
/// Errors
pub mod error;
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
use crate::windows::windows::WindowsInterface as Interface;

#[cfg(target_os = "linux")]
use crate::linux::linux::LinuxDBus as Interface;

use crate::{
    error::FlashResult,
    traits::Flasher,
};

/// The platform-native Flasher type
pub type OsFlasher = Flasher<Interface>;

/// Automatically creates a Flasher configured for the current operating system.
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
    let device = Interface;
    Ok(Flasher::new(device))
}
