use std::{
    fs::{
        self,
        File,
        OpenOptions,
    },
    io::Read,
    os::unix::fs::{
        FileExt,
        OpenOptionsExt,
    },
    path::PathBuf,
};

use rustix::{
    fs::FlockOperation,
    mount::UnmountFlags,
    path::Arg,
};
use tracing::{
    instrument,
    warn,
};

use crate::{
    data_types::{
        BlockDevice,
        MountedPartition,
    },
    error::{
        FlashError,
        FlashResult,
    },
    traits::{
        DeviceEjector,
        DeviceEnumerator,
        DeviceUnmounter,
        DeviceWriter,
        ImageSource,
        RawWriteHandle,
    },
};
/// Linux Raw File Handle
#[derive(Debug)]
pub struct LinuxRawWriteHandle {
    file: fs::File,
    phyisical_sector_size: u32,
    size_bytes: u64,
}
/// How to enumerate a device
#[derive(Debug)]
pub struct LinuxDeviceEnumerator;
/// The unmounter implementation
#[derive(Debug)]
pub struct LinuxDeviceUnmounter;
/// THe writer implementation
#[derive(Debug)]
pub struct LinuxDeviceWriter;
/// how the disk is ejected
#[derive(Debug)]
pub struct LinuxDeviceEjector;
/// How the file written is checked and information about it
#[derive(Debug)]
pub struct LinuxImageSource<R>
where
    R: Read,
{
    reader: R,
    uncompressed_size: Option<u64>,
    expected_hash: Option<[u8; 32]>,
}
impl<R: Read> LinuxImageSource<R> {
    /// default constructor
    pub fn new(reader: R, uncompressed_size: Option<u64>, expected_hash: Option<[u8; 32]>) -> Self {
        Self {
            reader,
            uncompressed_size,
            expected_hash,
        }
    }
}
impl LinuxDeviceEnumerator {
    /// name of the usb device
    // TODO: Check later if correct or remove
    #[instrument(ret)]
    fn name(mut path: PathBuf) -> Result<String, FlashError> {
        path.push("device/model");
        let file_output = fs::read_to_string(path)?;
        Ok(file_output.trim().into())
    }
    /// the dev path
    // TODO: Better error handeling with the string
    #[instrument(ret)]
    fn path(path: PathBuf) -> Option<PathBuf> {
        // 3 needed from getting /sys/block/xy the xy
        let path = path.components().nth(3)?;
        let mut output = PathBuf::new();
        output.push("/dev");
        output.push(&*path.to_string_lossy());
        Some(output)
    }
    /// gets physical sector size
    #[instrument(ret)]
    fn sector_size(mut path: PathBuf) -> Result<u32, FlashError> {
        path.push("queue/logical_block_size");
        let content_file = fs::read_to_string(path)?;
        let output = content_file.trim().parse::<u32>()?;
        Ok(output)
    }
    #[instrument(ret)]
    fn get_size_bytes(mut path: PathBuf) -> Result<u64, FlashError> {
        const SECTOR_SIZE: u64 = 512;
        path.push("size");
        let file_output = fs::read_to_string(path)?;
        let bytes_parsed = file_output
            .trim()
            .parse::<u64>()?
            .saturating_mul(SECTOR_SIZE);
        Ok(bytes_parsed)
    }
    /// if the storage device can be removed
    #[instrument(ret)]
    fn removable_status(mut path: PathBuf) -> Result<bool, FlashError> {
        path.push("removable");
        let read_status = fs::read_to_string(path)?;
        let output: u8 = read_status.trim().parse::<u8>()?;
        if output == 1 {
            return Ok(true);
        }
        Ok(false)
    }
}
impl DeviceEnumerator for LinuxDeviceEnumerator {
    #[instrument(ret)]
    fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        const SYS_PATH: &str = "/sys/block/";
        let block_devices_found = std::fs::read_dir(SYS_PATH)?;
        Ok(block_devices_found
            .filter_map(|entry| match entry {
                Ok(correct_entry) => {
                    let path = correct_entry.path();
                    let file_name = correct_entry.file_name();
                    let file_name_string_lossy = file_name.to_string_lossy();
                    if file_name_string_lossy.starts_with("loop")
                        || file_name_string_lossy.starts_with("ram")
                        || file_name_string_lossy.starts_with("zram")
                    {
                        return None;
                    }

                    let output = BlockDevice {
                        path: Self::path(path.clone())?,
                        name: Self::name(path).ok()?,
                        size_bytes: Self::get_size_bytes(correct_entry.path()).ok()?,
                        is_removable: Self::removable_status(correct_entry.path()).ok()?,
                        is_mounted: mounted_status(correct_entry.path()).ok()?,
                        sector_size: Self::sector_size(correct_entry.path()).ok()?,
                    };
                    Some(output)
                }
                Err(_) => None,
            })
            .collect())
    }
    // fn watch_devices(&self) -> FlashResult<std::sync::mpsc::Receiver<DeviceEvent>> {
    //     todo!()
    // }
}
#[allow(clippy::redundant_pattern)]
impl DeviceUnmounter for LinuxDeviceUnmounter {
    fn unmount_all(&self, device: &crate::data_types::BlockDevice) -> FlashResult<()> {
        rustix::mount::unmount(device.path.clone(), UnmountFlags::NOFOLLOW).map_err(|value| {
            match value.kind() {
                std::io::ErrorKind::NotFound => FlashError::DeviceNotFound(device.path.clone()),
                std::io::ErrorKind::PermissionDenied => FlashError::InsufficientPrivileges,
                std::io::ErrorKind::ResourceBusy => FlashError::DeviceBusy {
                    path: device.path.clone(),
                },
                error @ _ => FlashError::UnmountFailed {
                    device: device.path.clone(),
                    reason: error.to_string(),
                },
            }
        })
    }

    fn is_fully_unmounted(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> FlashResult<Option<Vec<MountedPartition>>> {
        mounted_status(device.path.clone())
    }
}
impl RawWriteHandle for LinuxRawWriteHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        self.file
            .write_all_at(buf, offset)
            .map_err(|source| FlashError::WriteFailed { offset, source })
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()> {
        self.file.read_exact_at(buf, offset).map_err(FlashError::Io)
    }

    fn flush_to_disk(&mut self) -> FlashResult<()> {
        self.file.sync_all().map_err(FlashError::Io)
    }

    fn sector_size(&self) -> u32 {
        self.phyisical_sector_size
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(self.size_bytes)
    }
}
impl DeviceWriter for LinuxDeviceWriter {
    type Handle = LinuxRawWriteHandle;

    #[allow(clippy::redundant_pattern)]
    fn open_for_writing(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> FlashResult<Self::Handle> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            //Linux 2.6 and later  if the block device is in use by the system like mounted fails with the error EBUSY
            .custom_flags(rustix::fs::OFlags::EXCL.bits().try_into()?)
            .open(device.path.clone())
            .map_err(|_| FlashError::InsufficientPrivileges)?;

        //TODO: test if flock works or ioctl is needed
        if let Err(error) = rustix::fs::flock(&file, FlockOperation::LockExclusive) {
            let error_back = match error.kind() {
                std::io::ErrorKind::NotFound => FlashError::DeviceNotFound(device.path.clone()),
                std::io::ErrorKind::PermissionDenied => FlashError::InsufficientPrivileges,
                std::io::ErrorKind::ResourceBusy => FlashError::DeviceBusy {
                    path: device.path.clone(),
                },
                error @ _ => FlashError::FileLockFailed {
                    device: device.path.clone(),
                    reason: error.to_string(),
                },
            };
            return Err(error_back);
        }

        Ok(LinuxRawWriteHandle {
            file,
            phyisical_sector_size: device.sector_size,
            size_bytes: device.size_bytes,
        })
    }
}
impl DeviceEjector for LinuxDeviceEjector {
    /// check that safe to eject device
    fn eject(&self, device: &crate::data_types::BlockDevice) -> FlashResult<()> {
        let file = File::open(device.path.clone())?;
        if rustix::fs::fsync(file).is_err() {
            return Err(FlashError::SyncError);
        }
        Ok(())
    }
}
impl<R: Read> ImageSource for LinuxImageSource<R> {
    fn uncompressed_size(&self) -> Option<u64> {
        self.uncompressed_size
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> FlashResult<usize> {
        self.reader.read(buf).map_err(FlashError::Io)
    }
    fn expected_hash(&self) -> Option<[u8; 32]> {
        self.expected_hash
    }
}
/// Check if mounted
#[instrument(ret)]
fn mounted_status(path: PathBuf) -> Result<Option<Vec<MountedPartition>>, FlashError> {
    let path_selected = path
        .components()
        .nth(3)
        .ok_or(FlashError::DeviceNotFound(path.clone()))?;
    let mounted_devices = fs::read_to_string("/proc/mounts")?;
    let device = path_selected
        .as_os_str()
        .to_str()
        .ok_or(FlashError::DeviceNotFound(path.clone()))?;
    let mut dev_path_search = PathBuf::new();
    dev_path_search.push("/dev");
    dev_path_search.push(device);
    if !dev_path_search.exists() {
        return Err(FlashError::DeviceNotFound(dev_path_search));
    }
    let search_string_from_path = dev_path_search.to_str();
    warn!("{:?}", search_string_from_path);
    if let Some(correct_device_path) = search_string_from_path {
        let output: Vec<MountedPartition> = mounted_devices
            .lines()
            .filter_map(|line| {
                let mut lines = line.split_whitespace();
                let device = lines.next()?;
                if !device.starts_with(correct_device_path) {
                    return None;
                };
                let mount_point = lines.next()?;
                Some(MountedPartition {
                    device_path: device.into(),
                    mount_point: mount_point.into(),
                })
            })
            .collect();
        // chekcs if a mounted partition is there
        if output.len() > 0 {
            return Ok(Some(output));
        }
    }
    Ok(None)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn list_devie() {
        let enumerator = LinuxDeviceEnumerator;
        let devices = LinuxDeviceEnumerator::list_devices(&enumerator);
        assert!(devices.is_ok());
        let devices = devices.unwrap();
        if let Some(checked) = devices.get(0) {
            let corrected_path = {
                let path = checked.path.components().nth(2).unwrap();
                let mut new_path = PathBuf::from("/sys/block");
                new_path.push(path);
                new_path
            };
            assert!(LinuxDeviceEnumerator::name(corrected_path.clone()).is_ok());
            assert!(LinuxDeviceEnumerator::path(corrected_path.clone()).is_some());
            assert!(LinuxDeviceEnumerator::get_size_bytes(corrected_path.clone()).is_ok());
            assert!(LinuxDeviceEnumerator::removable_status(corrected_path.clone()).is_ok());
        }
    }
}
