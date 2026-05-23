use std::{
    fmt::Display,
    path::{
        Path,
        PathBuf,
    },
};

/// Storage Device
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockDevice {
    /// Platform-native path: /dev/sdb, /dev/rdisk2, \\.\PhysicalDrive1
    pub path: PathBuf,
    /// Human readable name: "Samsung USB Drive"
    pub name: String,
    /// the size in bytes see get_size
    //TODO: Fix link to size here
    pub size_bytes: u64,
    /// Check if removable
    pub is_removable: bool,
    /// Sector size
    pub sector_size: usize,
}

/// How the async file written is checked and information about it
#[allow(dead_code)]
#[derive(Debug)]
pub struct AsyncImageSourceFile {
    /// file pointer
    pub file: tokio::fs::File,
    /// size end of the iso
    uncompressed_size: u64,
    /// hash checked after file input matches
    expected_hash: Option<[u8; 32]>,
}
/// Information about Mounted Partition
#[derive(Debug, Clone)]
pub struct MountedPartition {
    /// path
    ///
    /// example: /dev/sda
    pub device_path: PathBuf,
    /// on device path
    ///
    ///example:  /media/user/BOOT
    pub mount_point: PathBuf,
}

/// The progress of flashing to the device
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct FlashProgress {
    pub bytes_written: u64,
    pub total_bytes: u64, // 0 if unknown (e.g. streaming xz)
    pub bytes_per_sec: f64,
    pub phase: FlashPhase,
}

#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub enum FlashPhase {
    Preparing,
    Unmounting,
    Writing,
    Flushing,
    Verifying,
    VerifyingHashDoesNotMatch,
    Done,
}

/// The Devices Size rounded down
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub enum Size {
    Bytes(u64),
    KiloByte(u64),
    MegaByte(u64),
    GigaByte(u64),
}
/// Events to devices
#[allow(missing_docs)]
#[derive(Debug)]
pub enum DeviceEvent {
    Added(BlockDevice),
    Removed(PathBuf),
}
impl FlashProgress {
    /// how the flash transisiton states
    #[must_use]
    pub fn transition(phase_state: FlashPhase) -> Self {
        FlashProgress {
            bytes_written: 0,
            total_bytes: 0,
            bytes_per_sec: 0.0,
            phase: phase_state,
        }
    }
}
impl BlockDevice {
    /// constructor
    #[must_use]
    pub fn new(
        path: PathBuf,
        name: String,
        size_bytes: u64,
        is_removable: bool,
        sector_size: usize,
    ) -> Self {
        Self {
            path,
            name,
            size_bytes,
            is_removable,
            sector_size,
        }
    }
    /// Gets the byte size and rounded down
    #[must_use]
    pub fn get_sizes(&self) -> Size {
        calculate_size_from_bytes(self.size_bytes)
    }
    /// gives name back
    #[must_use]
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// gives size in bytes aback
    #[must_use]
    #[inline]
    pub fn size_in_bytes(&self) -> u64 {
        self.size_bytes
    }
    /// gives back if the device is removable usb ...
    #[must_use]
    #[inline]
    pub fn removable(&self) -> bool {
        self.is_removable
    }
    /// gives physical sector size back
    #[must_use]
    #[inline]
    pub fn sector_size(&self) -> usize {
        self.sector_size
    }
}
impl AsyncImageSourceFile {
    /// default constructor
    pub fn new(
        file: tokio::fs::File,
        uncompressed_size: u64,
        expected_hash: Option<[u8; 32]>,
    ) -> Self {
        Self {
            file,
            uncompressed_size,
            expected_hash,
        }
    }
}

/// `ImageSource` for the async variant: provides metadata.
///
/// `read_chunk` is intentionally unused — `Flasher::flash` reads via
/// `AsyncBufRead`; calling `read_chunk` on this type is a programming error.
impl AsyncImageSourceFile {
    /// size
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Hash
    pub fn expected_hash(&self) -> Option<[u8; 32]> {
        self.expected_hash
    }
}

impl MountedPartition {
    /// gives device path back
    #[must_use]
    #[inline]
    pub fn device_path(&self) -> &Path {
        &self.device_path
    }
    /// gives mount point back
    #[must_use]
    #[inline]
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }
}
impl FlashProgress {
    /// gives bytes written back
    #[must_use]
    #[inline]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
    /// gives total bytes back
    #[must_use]
    #[inline]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
    /// gives byte per second back
    #[must_use]
    #[inline]
    pub fn bytes_per_sec(&self) -> f64 {
        self.bytes_per_sec
    }
    /// gives the phase in is now back
    #[must_use]
    #[inline]
    pub fn phase(&self) -> FlashPhase {
        self.phase.clone()
    }
}
impl Display for Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Size::Bytes(x) => write!(f, "{} B", x),
            Size::KiloByte(x) => write!(f, "{} KB", x),
            Size::MegaByte(x) => write!(f, "{} MB", x),
            Size::GigaByte(x) => write!(f, "{} GB", x),
        }
    }
}

/// generates from bytes rounded biggest value
#[must_use]
pub fn calculate_size_from_bytes(bytes_size: u64) -> Size {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    match bytes_size {
        b if b >= GB => Size::GigaByte(b.div_ceil(GB)),
        b if b >= MB => Size::MegaByte(b.div_ceil(MB)),
        b if b >= KB => Size::KiloByte(b.div_ceil(KB)),
        b => Size::Bytes(b),
    }
}
