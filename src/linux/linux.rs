use std::{
    collections::HashMap,
    io::SeekFrom,
    path::Path,
    pin::Pin,
    task::{
        Context,
        Poll,
    },
};

use tokio::io::{
    AsyncReadExt,
    AsyncSeekExt,
    AsyncWriteExt,
    ReadBuf,
};
use zbus::zvariant::{
    OwnedObjectPath,
    OwnedValue,
};
use zvariant::{
    OwnedFd,
    Value,
};

use crate::{
    data_types::BlockDevice,
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
pub struct LinuxRawWriteHandle {
    file: tokio::fs::File,
    sector_size: usize,
    size_bytes: u64,
}
#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Manager",
    default_service = "org.freedesktop.UDisks2",
    default_path = "/org/freedesktop/UDisks2/Manager"
)]
trait UDisks2Manager {
    #[zbus(name = "GetBlockDevices")]
    fn get_block_devices(
        &self,
        options: &HashMap<String, OwnedValue>,
    ) -> zbus::Result<Vec<OwnedObjectPath>>;
}
#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Block",
    default_service = "org.freedesktop.UDisks2"
)]
trait UDisks2Block {
    #[zbus(name = "OpenDevice")]
    fn open_device(
        &self,
        mode: &str,
        options: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;

    #[zbus(property)]
    fn device(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn size(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn hw_sector_size(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn drive(&self) -> zbus::Result<zbus::zvariant::ObjectPath<'_>>;
}
#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Drive",
    default_service = "org.freedesktop.UDisks2"
)]
trait UDisks2Drive {
    #[zbus(property)]
    fn model(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn removable(&self) -> zbus::Result<bool>;
    #[zbus(name = "Eject")]
    fn eject(
        &self,
        options: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}
#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Filesystem",
    default_service = "org.freedesktop.UDisks2"
)]
trait UDisks2Filesystem {
    #[zbus(property)]
    fn mount_points(&self) -> zbus::Result<Vec<Vec<u8>>>;
    #[zbus(name = "Unmount")]
    fn unmount(
        &self,
        options: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}

/// Linux a connection to the d bus system bus
#[derive(Debug, Clone)]
pub struct LinuxDBus {
    connection: zbus::Connection,
}

impl LinuxDBus {
    #[allow(missing_docs)]
    pub async fn new() -> FlashResult<Self> {
        let connection = zbus::Connection::system().await?;
        Ok(Self { connection })
    }
}
impl DeviceEnumerator for LinuxDBus {
    async fn list_devices(&self) -> FlashResult<Vec<crate::data_types::BlockDevice>> {
        let manager = UDisks2ManagerProxy::new(&self.connection).await?;
        let all_devices = manager.get_block_devices(&HashMap::new()).await?;
        let mut devices = Vec::new();
        for obj_path in all_devices {
            let block_proxy = UDisks2BlockProxy::builder(&self.connection)
                .path(obj_path.as_str())?
                .build()
                .await?;

            let device_bytes = block_proxy.device().await.unwrap_or_default(); // Vec<u8>
            let path_str = String::from_utf8_lossy(&device_bytes).to_string();

            let trimmed = path_str.trim_end_matches('\0');
            let path = std::path::PathBuf::from(&trimmed);
            if trimmed.starts_with("/dev/loop")
                || trimmed.starts_with("/dev/ram")
                || trimmed.starts_with("/dev/zram")
                || trimmed.chars().last().unwrap_or(' ').is_ascii_digit()
            {
                continue;
            }

            let size_bytes = block_proxy.size().await.unwrap_or(0);
            let sector_size = block_proxy.hw_sector_size().await.unwrap_or(512) as usize;
            let drive_obj_path = block_proxy.drive().await?;
            let drive_proxy = UDisks2DriveProxy::builder(&self.connection)
                .path(drive_obj_path.as_str())?
                .build()
                .await?;
            let name = drive_proxy
                .model()
                .await
                .unwrap_or_else(|_| "<unknown>".to_string());
            let is_removable = drive_proxy.removable().await.unwrap_or(false);

            let bd = BlockDevice {
                path,
                name,
                size_bytes,
                is_removable,
                sector_size,
            };
            devices.push(bd);
        }
        Ok(devices)
    }
}
impl DeviceUnmounter for LinuxDBus {
    async fn unmount(&self, device: &BlockDevice) -> FlashResult<()> {
        let dev_filename = device
            .path
            .file_name()
            .ok_or_else(|| FlashError::DeviceBusy {
                path: device.path.clone(),
            })?
            .to_string_lossy();
        let dev_obj_path = format!("/org/freedesktop/UDisks2/block_devices/{}", dev_filename);

        let manager = UDisks2ManagerProxy::new(&self.connection).await?;
        let all_devices = manager.get_block_devices(&HashMap::new()).await?;

        let disk_block_proxy = UDisks2BlockProxy::builder(&self.connection)
            .path(dev_obj_path.as_str())?
            .build()
            .await?;
        let drive_obj_path = disk_block_proxy.drive().await?;

        for obj_path in all_devices {
            let block_proxy = match UDisks2BlockProxy::builder(&self.connection)
                .path(obj_path.as_str())?
                .build()
                .await
            {
                Ok(proxy) => proxy,
                Err(_) => continue,
            };

            let their_drive = block_proxy.drive().await.ok();
            if their_drive != Some(drive_obj_path.clone()) {
                continue;
            }

            let child_dev_bytes = block_proxy.device().await.unwrap_or_default();
            let child_dev_str = String::from_utf8_lossy(&child_dev_bytes);
            if Path::new(child_dev_str.trim_end_matches("\0")) == device.path {
                continue;
            }

            if let Ok(fs_proxy) = UDisks2FilesystemProxy::builder(&self.connection)
                .path(obj_path.as_str())?
                .build()
                .await
            {
                let mps = fs_proxy.mount_points().await.unwrap_or_default();
                if !mps.is_empty() {
                    // Mounted, so unmount
                    let mut unmount_options = HashMap::new();

                    // UDisks2 expects boolean values wrapped as an OwnedValue variant
                    let force_val = zbus::zvariant::Value::from(true).try_into_owned()?;
                    unmount_options.insert("force".to_string(), force_val);
                    fs_proxy.unmount(&unmount_options).await.map_err(|_| {
                        FlashError::DeviceBusy {
                            path: child_dev_str.trim_end_matches("\0").into(),
                        }
                    })?;
                }
            }
        }
        Ok(())
    }
}
impl DeviceEjector for LinuxDBus {
    async fn eject(&self, device: &BlockDevice) -> FlashResult<()> {
        let dev_filename = device
            .path
            .file_name()
            .ok_or_else(|| FlashError::DeviceBusy {
                path: device.path.clone(),
            })?
            .to_string_lossy();
        let dev_obj_path = format!("/org/freedesktop/UDisks2/block_devices/{}", dev_filename);

        let block_proxy = UDisks2BlockProxy::builder(&self.connection)
            .path(dev_obj_path.as_str())?
            .build()
            .await?;

        let drive_obj_path = block_proxy.drive().await?;

        let drive_proxy = UDisks2DriveProxy::builder(&self.connection)
            .path(drive_obj_path.as_str())?
            .build()
            .await?;

        drive_proxy
            .eject(&HashMap::new())
            .await
            .map_err(|_| FlashError::DeviceBusy {
                path: device.path.clone(),
            })?;

        Ok(())
    }
}
impl RawWriteHandle for LinuxRawWriteHandle {
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .await
            .map_err(FlashError::Io)?;
        self.file.write_all(buf).await.map_err(FlashError::Io)?;

        Ok(())
    }

    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .await
            .map_err(FlashError::Io)?;
        self.file.read_exact(buf).await.map_err(FlashError::Io)?;
        Ok(())
    }

    async fn flush_to_disk(&mut self) -> FlashResult<()> {
        self.file.sync_all().await?;
        Ok(())
    }

    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(self.size_bytes)
    }

    async fn seek(&mut self, seek: SeekFrom) -> FlashResult<()> {
        self.file.seek(seek).await.map_err(FlashError::Io)?;
        Ok(())
    }
}
impl DeviceWriter for LinuxDBus {
    type Handle = LinuxRawWriteHandle;

    async fn open_for_writing(&self, device: &BlockDevice) -> FlashResult<Self::Handle> {
        let dev_filename = device
            .path
            .file_name()
            .ok_or_else(|| FlashError::DeviceNotFound(device.path.clone()))?
            .to_string_lossy();
        let dev_obj_path = format!("/org/freedesktop/UDisks2/block_devices/{}", dev_filename);

        // 2. Create UDisks2BlockProxy for it
        let block_proxy = UDisks2BlockProxy::builder(&self.connection)
            .path(dev_obj_path.as_str())?
            .build()
            .await?;

        // the flags O_DIRECT  O_SYNC  O_CLOEXEC must be there for raw writing and that no kernel caching is used
        let open_flags = libc::O_DIRECT | libc::O_SYNC | libc::O_CLOEXEC;

        let mut options = HashMap::new();
        let flags_owned = Value::from(open_flags).try_into_owned()?;

        options.insert("flags".to_string(), flags_owned); // 3. Open device via D-Bus
        let fd: OwnedFd = block_proxy
            .open_device("rw", &options)
            .await
            .map_err(FlashError::Zbus)?;
        let std_fd: std::os::fd::OwnedFd = fd.into();
        let file = tokio::fs::File::from(std::fs::File::from(std_fd));

        Ok(LinuxRawWriteHandle {
            file,
            sector_size: device.sector_size,
            size_bytes: device.size_bytes,
        })
    }
}
impl tokio::io::AsyncRead for LinuxRawWriteHandle {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for LinuxRawWriteHandle {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.file).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_shutdown(cx)
    }
}

impl tokio::io::AsyncSeek for LinuxRawWriteHandle {
    fn start_seek(mut self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
        Pin::new(&mut self.file).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
        Pin::new(&mut self.file).poll_complete(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_get() {
        let test = LinuxDBus::new().await.unwrap();
        test.list_devices().await.unwrap();
    }
}
