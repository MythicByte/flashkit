use crate::traits::{
    DeviceEjector,
    DeviceEnumerator,
    DeviceUnmounter,
    RawWriteHandle,
};

#[allow(missing_docs)]
#[derive(Debug)]
pub struct WindowsRawWriteHandle {
    file: tokio::fs::File,
    sector_size: usize,
    size_bytes: u64,
}
impl RawWriteHandle for WindowsRawWriteHandle {
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> crate::error::FlashResult<()> {
        todo!()
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> crate::error::FlashResult<()> {
        todo!()
    }

    async fn flush_to_disk(&mut self) -> crate::error::FlashResult<()> {
        todo!()
    }

    fn sector_size(&self) -> usize {
        todo!()
    }

    fn size_bytes(&self) -> crate::error::FlashResult<u64> {
        todo!()
    }

    async fn seek(&mut self, seek: std::io::SeekFrom) -> crate::error::FlashResult<()> {
        todo!()
    }
}
impl DeviceEnumerator for WindowsRawWriteHandle {
    async fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        todo!()
    }
}
impl DeviceEjector for WindowsRawWriteHandle {
    async fn eject(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> crate::error::FlashResult<()> {
        todo!()
    }
}
impl DeviceUnmounter for WindowsRawWriteHandle {
    async fn unmount(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> crate::error::FlashResult<()> {
        todo!()
    }
}
