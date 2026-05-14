use std::{
    io::Read,
    path::PathBuf,
};

use crate::{
    error::{
        FlashError,
        FlashResult,
    },
    traits::ImageSource,
};

/// Storage Device
#[derive(Debug, Clone)]
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
    /// Check if its mounted
    pub is_mounted: Option<Vec<MountedPartition>>,
    /// Sector size
    pub sector_size: u32,
}

/// How the file written is checked and information about it
#[derive(Debug)]
pub struct ImageSourceFile<R>
where
    R: Read,
{
    reader: R,
    uncompressed_size: u64,
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
    pub fn phase(phase: FlashPhase) -> Self {
        FlashProgress {
            bytes_written: 0,
            total_bytes: 0,
            bytes_per_sec: 0.0,
            phase,
        }
    }
}
impl BlockDevice {
    /// Gets the byte size and rounded down
    #[must_use]
    pub fn get_sizes(&self) -> Size {
        calculate_size_from_bytes(self.size_bytes)
    }
}
impl<R: Read> ImageSourceFile<R> {
    /// default constructor
    pub fn new(reader: R, uncompressed_size: u64, expected_hash: Option<[u8; 32]>) -> Self {
        Self {
            reader,
            uncompressed_size,
            expected_hash,
        }
    }
}
impl<R: Read> ImageSource for ImageSourceFile<R> {
    fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> FlashResult<usize> {
        self.reader.read(buf).map_err(FlashError::Io)
    }
    fn expected_hash(&self) -> Option<[u8; 32]> {
        self.expected_hash
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
