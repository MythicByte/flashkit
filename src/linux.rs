use crate::{
    data_types::DeviceEvent,
    error::FlashResult,
    traits::{DeviceEjector, DeviceEnumerator, DeviceUnmounter, DeviceWriter},
};

pub struct LinuxDeviceEnumerator;
pub struct LinuxDeviceUnmounter;
pub struct LinuxDeviceWriter;
pub struct LinuxDeviceEjector;

impl DeviceEnumerator for LinuxDeviceEnumerator {
    fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        todo!()
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
