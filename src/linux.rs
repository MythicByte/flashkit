use std::{
    fs,
    path::PathBuf,
};

use rustix::{
    mount::UnmountFlags,
    path::Arg,
};

use crate::{
    data_types::{
        BlockDevice,
        DeviceEvent,
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

pub struct LinuxRawWriteHandle;
pub struct LinuxDeviceEnumerator;
pub struct LinuxDeviceUnmounter;
pub struct LinuxDeviceWriter;
pub struct LinuxDeviceEjector;
pub struct LinuxImageSource;

impl LinuxDeviceEnumerator {
    /// name of the usb device
    // TODO: Check later if correct or remove
    fn name(mut path: PathBuf) -> Result<String, FlashError> {
        path.push("/device/model");
        let file_output = fs::read_to_string(path)?;
        Ok(file_output)
    }
    /// the dev path
    // TODO: Better error handeling with the string
    fn path(path: PathBuf) -> Option<String> {
        let path = path.components().nth(1)?;
        let mut output = String::with_capacity(10);
        output.push_str("/dev");
        output.push_str(&path.to_string_lossy());
        Some(output)
    }
    /// gets physical sector size
    fn sector_size(mut path: PathBuf) -> Result<u16, FlashError> {
        path.push("/queue/logical_block_size");
        let content_file = fs::read_to_string(path)?;
        let output = content_file.parse::<u16>()?;
        Ok(output)
    }
    fn get_size_bytes(mut path: PathBuf) -> Result<u64, FlashError> {
        path.push("size");
        let file_output = fs::read_to_string(path)?;
        let bytes_parsed = file_output.parse::<u64>()?;
        Ok(bytes_parsed)
    }
    /// if the storage device can be removed
    fn removable_status(mut path: PathBuf) -> Result<bool, FlashError> {
        path.push("removable");
        let read_status = fs::read_to_string(path)?;
        let output: u8 = read_status.parse::<u8>()?;
        if output == 1 {
            return Ok(true);
        }
        Ok(false)
    }
}
impl DeviceEnumerator for LinuxDeviceEnumerator {
    fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        const SYS_PATH: &str = "/sys/block/";
        let block_devices_found = std::fs::read_dir(SYS_PATH)?;
        Ok(block_devices_found
            .filter_map(|entry| match entry {
                Ok(correct_entry) => {
                    let path = correct_entry.path();

                    let output = BlockDevice {
                        path: path.clone(),
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
    fn watch_devices(&self) -> FlashResult<std::sync::mpsc::Receiver<DeviceEvent>> {
        todo!()
    }
}
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

    fn is_fully_unmounted(&self, device: &crate::data_types::BlockDevice) -> FlashResult<bool> {
        mounted_status(device.path.clone())
    }
}
impl RawWriteHandle for LinuxRawWriteHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        todo!()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()> {
        todo!()
    }

    fn flush_to_disk(&mut self) -> FlashResult<()> {
        todo!()
    }

    fn sector_size(&self) -> u64 {
        todo!()
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        todo!()
    }
}
impl DeviceWriter for LinuxDeviceWriter {
    type Handle;

    fn open_for_writing(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> FlashResult<Self::Handle> {
        todo!()
    }
}
impl DeviceEjector for LinuxDeviceEjector {
    fn eject(&self, device: &crate::data_types::BlockDevice) -> FlashResult<()> {
        todo!()
    }
}
impl ImageSource for LinuxImageSource {
    fn uncompressed_size(&self) -> Option<u64> {
        todo!()
    }

    fn read_chunk(&mut self, buf: &mut [u8]) -> FlashResult<usize> {
        todo!()
    }
}
/// Check if mounted
fn mounted_status(path: PathBuf) -> Result<bool, FlashError> {
    let path_selected = path
        .components()
        .nth(1)
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
        return Ok(false);
    }
    let search_string_from_path = dev_path_search.to_str();
    if let Some(correct_device_path) = search_string_from_path {
        mounted_devices
            .lines()
            .find(|line| correct_device_path == *line);
    }
    Ok(false)
}
