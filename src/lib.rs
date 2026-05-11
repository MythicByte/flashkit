//! A libary for flashing data to a storage device / block device
//!
//! **Cross Platform Support**
//!
//! # Supported
//! - [ ] Linux
//! - [ ] Windows
//! - [ ] Macos
//!

/// Libary Data types
pub(crate) mod data_types;
/// Errors
pub(crate) mod error;
#[cfg(target_os = "linux")]
/// Linux
pub(crate) mod linux;
#[cfg(target_os = "macos")]
/// Macos
pub(crate) mod macos;
/// Generic Traits abstraction
pub(crate) mod traits;
#[cfg(target_os = "windows")]
/// Windows
pub(crate) mod windows;
