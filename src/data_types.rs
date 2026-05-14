use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct BlockDevice {
    /// Platform-native path: /dev/sdb, /dev/rdisk2, \\.\PhysicalDrive1
    pub path: PathBuf,
    /// Human readable name: "Samsung USB Drive"
    pub name: String,
    pub size_bytes: u64,
    /// Check if removable
    pub is_removable: bool,
    /// Check if its mounted
    pub is_mounted: bool,
    /// Sector size
    pub sector_size: u32,
}

#[derive(Debug, Clone)]
pub struct MountedPartition {
    pub device_path: PathBuf, //  /dev/sdb1
    pub mount_point: PathBuf, //  /media/user/BOOT
}

#[derive(Debug, Clone)]
pub struct FlashProgress {
    pub bytes_written: u64,
    pub total_bytes: u64, // 0 if unknown (e.g. streaming xz)
    pub bytes_per_sec: f64,
    pub phase: FlashPhase,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FlashPhase {
    Preparing,
    Unmounting,
    Writing,
    Flushing,
    Verifying,
    Done,
}

/// The Devices Size rounded
#[derive(Debug, Clone, PartialEq)]
pub enum Size {
    Bytes(u64),
    KiloByte(u64),
    MegaByte(u64),
    GigaByte(u64),
}

/// Events to devices
#[derive(Debug)]
pub enum DeviceEvent {
    Added(BlockDevice),
    Removed(PathBuf),
}
impl FlashProgress {
    pub fn phase(phase: FlashPhase) -> Self {
        FlashProgress {
            bytes_written: 0,
            total_bytes: 0,
            bytes_per_sec: 0.0,
            phase,
        }
    }
}
/// generates from bytes rounded biggest value
pub(crate) fn calculate_size_from_bytes(bytes_size: u64) -> Size {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    match bytes_size {
        b if b >= GB => Size::GigaByte(b.saturating_div(GB)),
        b if b >= MB => Size::MegaByte(b.saturating_div(MB)),
        b if b >= KB => Size::KiloByte(b.saturating_div(KB)),
        b => Size::Bytes(b),
    }
}
