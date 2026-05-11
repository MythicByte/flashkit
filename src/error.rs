use std::path::PathBuf;

use thiserror::Error;
/// Generic Result Type
pub type FlashResult<T> = Result<T, FlashError>;

/// Errors
#[derive(Debug, Error)]
pub enum FlashError {
    #[error("Insufficient privileges — run as root or administrator")]
    InsufficientPrivileges,

    #[error("Device not found: {0}")]
    DeviceNotFound(PathBuf),

    #[error("Device is busy: {path} mounted at {mount_point}")]
    DeviceBusy { path: PathBuf, mount_point: PathBuf },

    #[error("Unmount failed for {device}: {reason}")]
    UnmountFailed { device: PathBuf, reason: String },

    #[error("Write failed at offset {offset}: {source}")]
    WriteFailed { offset: u64, source: std::io::Error },

    #[error(
        "Verification failed — SHA256 mismatch with {failed_hash_hex} should have matched {expected_hex}"
    )]
    VerificationFailed {
        failed_hash_hex: String,
        expected_hex: String,
    },

    #[error("Libary feature not for this backend implemented")]
    UnsportedFeature,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
