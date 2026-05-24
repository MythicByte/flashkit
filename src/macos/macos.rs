use std::{
    io::{
        Seek,
        SeekFrom,
    },
    os::fd::{
        AsRawFd,
        BorrowedFd,
    },
};

use crate::{
    error::{
        FlashError,
        FlashResult,
    },
    traits::{
        DeviceEjector,
        DeviceEnumerator,
        DeviceUnmounter,
        DeviceWriter,
        RawWriteHandle,
    },
};
#[allow(missing_docs)]
#[derive(Debug)]
pub struct DarwinInterface;

#[allow(missing_docs)]
#[derive(Debug)]
pub struct DarwinRawWriteHandle {
    file: std::fs::File,
    sector_size: usize,
    size_bytes: u64,
}
impl RawWriteHandle for DarwinRawWriteHandle {
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        let fd = self.file.as_raw_fd();
        let ptr = buf.as_ptr() as usize;
        let len = buf.len();

        tokio::task::spawn_blocking(move || {
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };

            // Perform a positional write directly using the OS file descriptor
            // This is completely compatible with O_DIRECT requirements
            // Safety: reconstruct a BorrowedFd with a local lifetime inside the closure
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
            rustix::io::pwrite(borrowed_fd, slice, offset)
        })
        .await
        .map_err(|_| FlashError::SyncError)?
        .map_err(std::io::Error::from)?; // Converts rustix error into std::io::Error
        Ok(())
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize> {
        let fd = self.file.as_raw_fd();
        let ptr = buf.as_mut_ptr() as usize;
        let len = buf.len();

        let bytes_read = tokio::task::spawn_blocking(move || {
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
            let borrowed_fd = unsafe { BorrowedFd::borrow_raw(fd) };
            rustix::io::pread(borrowed_fd, slice, offset)
        })
        .await
        .map_err(|_| FlashError::SyncError)?
        .map_err(std::io::Error::from)?;
        Ok(bytes_read)
    }

    async fn flush_to_disk(&mut self) -> FlashResult<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(self.size_bytes)
    }

    async fn seek(&mut self, seek: SeekFrom) -> FlashResult<()> {
        self.file.seek(seek).map_err(FlashError::Io)?;
        Ok(())
    }
}
impl DeviceWriter for DarwinInterface {
    type Handle = DarwinRawWriteHandle;

    async fn open_for_writing(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> crate::error::FlashResult<Self::Handle> {
        todo!()
    }
}
impl DeviceEnumerator for DarwinInterface {
    async fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        todo!()
    }
}
impl DeviceEjector for DarwinInterface {
    async fn eject(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> crate::error::FlashResult<()> {
        let out = tokio::process::Command::new("diskutil")
            .args(["eject", device.path.to_string_lossy().as_ref()])
            .output()
            .await
            .map_err(FlashError::Io)?;

        if !out.status.success() {
            return Err(FlashError::DeviceBusy {
                path: device.path.clone(),
            });
        }

        Ok(())
    }
}
impl DeviceUnmounter for DarwinInterface {
    async fn unmount(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> crate::error::FlashResult<()> {
        let out = tokio::process::Command::new("diskutil")
            .args(["unmountDisk", device.path.to_string_lossy().as_ref()])
            .output()
            .await
            .map_err(FlashError::Io)?;

        if !out.status.success() {
            let reason = String::from_utf8_lossy(&out.stderr).to_string();
            return Err(FlashError::UnmountFailed {
                device: device.path.clone(),
                reason,
            });
        }

        Ok(())
    }
}
