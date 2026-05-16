use std::{
    io::Read,
    path::{
        Path,
        PathBuf,
    },
};

use tokio::io::{
    AsyncRead,
    AsyncReadExt,
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
    pub sector_size: usize,
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
/// How the async file written is checked and information about it
#[allow(dead_code)]
#[derive(Debug)]
pub struct AsyncImageSourceFile<R>
where
    R: AsyncRead + AsyncReadExt,
{
    /// file pointer
    reader: R,
    /// size end of the iso
    uncompressed_size: u64,
    /// hash
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
        is_mounted: Option<Vec<MountedPartition>>,
        sector_size: usize,
    ) -> Self {
        Self {
            path,
            name,
            size_bytes,
            is_removable,
            is_mounted,
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
    /// gives mounted points back or None then none mounted
    #[must_use]
    #[inline]
    pub fn mounted(&self) -> Option<Vec<MountedPartition>> {
        self.is_mounted.clone()
    }
    /// gives physical sector size back
    #[must_use]
    #[inline]
    pub fn sector_size(&self) -> usize {
        self.sector_size
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
impl<R: AsyncRead + AsyncReadExt> AsyncImageSourceFile<R> {
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
impl<R: std::io::Read> std::io::Read for ImageSourceFile<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
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
