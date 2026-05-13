use crate::{
    data_types::{BlockDevice, DeviceEvent},
    error::FlashResult,
    traits::{
        DeviceEjector, DeviceEnumerator, DeviceUnmounter, DeviceWriter, ImageSource, RawWriteHandle,
    },
};

pub struct LinuxRawWriteHandle;
pub struct LinuxDeviceEnumerator;
pub struct LinuxDeviceUnmounter;
pub struct LinuxDeviceWriter;
pub struct LinuxDeviceEjector;
pub struct LinuxImageSource;

impl DeviceEnumerator for LinuxDeviceEnumerator {
    fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        const SYS_PATH: &str = "/sys/block/";
        let block_devices_found = std::fs::read_dir(SYS_PATH)?;
        Ok(block_devices_found
            .filter_map(|entry| match entry {
                Ok(correct_entry) => Some(BlockDevice {
                    path: correct_entry.path(),
                    name: correct_entry.file_name(),
                    size_bytes: todo!(),
                    is_removable: todo!(),
                    is_mounted: todo!(),
                    partitions: todo!(),
                    sector_size: todo!(),
                }),
                Err(_) => None,
            })
            .collect())
    }
    fn watch_devices(&self) -> FlashResult<std::sync::mpsc::Receiver<DeviceEvent>> {
        todo!()
    }
}
impl DeviceUnmounter for LinuxDeviceUnmounter {
    fn unmount_all(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> FlashResult<Vec<crate::data_types::MountedPartition>> {
        todo!()
    }

    fn is_fully_unmounted(&self, device: &crate::data_types::BlockDevice) -> FlashResult<bool> {
        todo!()
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
