use std::path::PathBuf;

use thiserror::Error;
/// Generic Result Type
pub type FlashResult<T> = Result<T, FlashError>;

/// Errors
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum FlashError {
    #[error("Insufficient privileges — run as root or administrator")]
    InsufficientPrivileges,

    #[error("The hash sh2 does not match")]
    Sha2512HashDoesNotMatch,

    #[error("Device not found: {0}")]
    DeviceNotFound(PathBuf),

    #[error("Device is busy: {path} ")]
    DeviceBusy { path: PathBuf },

    #[error("Unmount failed for {device}: {reason}")]
    UnmountFailed { device: PathBuf, reason: String },

    #[error("Write failed at offset {offset}: {source}")]
    WriteFailed { offset: u64, source: std::io::Error },

    #[error("Get exclusiv file lock failed for {device}: {reason}")]
    FileLockFailed { device: PathBuf, reason: String },
    #[error(
        "Verification failed — SHA256 mismatch with {failed_hash_hex} should have matched {expected_hex}"
    )]
    VerificationFailed {
        failed_hash_hex: String,
        expected_hex: String,
    },

    #[error("A error with synchronisation has accourd")]
    SyncError,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),

    #[error(transparent)]
    TryInt(#[from] std::num::TryFromIntError),

    #[error("Sending with channel failed")]
    SendChannelError,

    #[error("Filesystem has send a error")]
    FilesystemError(String),
    #[error("Allocation Layout error")]
    Layour(#[from] std::alloc::LayoutError),

    #[error("A array was accesed out of bounds")]
    OutOfBoundsArray,

    #[cfg(target_os = "windows")]
    #[error(transparent)]
    WindowsError(#[from] windows::core::Error),
    // Linux Errors only with D-bus
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    ZvariantError(#[from] zvariant::Error),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Zbus(#[from] zbus::Error),
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Fdo(#[from] zbus::fdo::Error),
}
