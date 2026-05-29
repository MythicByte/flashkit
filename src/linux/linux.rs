use std::{
    collections::HashMap,
    io::{
        Seek,
        SeekFrom,
    },
    path::{
        Path,
        PathBuf,
    },
};
use tokio_stream::{
    StreamExt,
    wrappers::ReceiverStream,
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
    data_types::{
        BlockDevice,
        DeviceEvent,
    },
    error::{
        FlashError,
        FlashResult,
    },
    traits::{
        AsyncDeviceEnumerator,
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
    file: std::fs::File,
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
#[zbus::proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.freedesktop.UDisks2",
    default_path = "/org/freedesktop/UDisks2"
)]
trait UDisks2ObjectManager {
    #[zbus(signal)]
    fn interfaces_added(
        &self,
        object_path: zbus::zvariant::ObjectPath<'_>,
        interfaces_and_properties: std::collections::HashMap<
            String,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        >,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn interfaces_removed(
        &self,
        object_path: zbus::zvariant::ObjectPath<'_>,
        interfaces: Vec<String>,
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
impl AsyncDeviceEnumerator for LinuxDBus {
    type WatchStream = ReceiverStream<DeviceEvent>;
    async fn watch_devices(&self) -> FlashResult<Self::WatchStream> {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let manager = UDisks2ObjectManagerProxy::new(&self.connection).await?;

        let mut added_stream = manager.receive_interfaces_added().await?;
        let mut removed_stream = manager.receive_interfaces_removed().await?;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                                    // 1. Handle newly connected devices
                Some(added) = added_stream.next() => {
                    let added_args = match added.args().map_err(FlashError::from)?;
                                        if let Some(block_props) = added_args.interfaces_and_properties.get("org.freedesktop.UDisks2.Block") {
                                            // Extract the raw system path string from the "Device" byte array property (ay)
                                            if let Some(device_val) = block_props.get("Device") {
                                                if let Ok(device_bytes) = <Vec<u8>>::try_from(device_val.clone()) {
                                                    let path_str = String::from_utf8_lossy(&device_bytes).trim_matches('\0').to_string();
                                                    let path = PathBuf::from(path_str);

                                                    // Extract device capacity size (t -> u64)
                                                    let size_bytes = block_props.get("Size")
                                                        .and_then(|v| u64::try_from(v.clone()).ok())
                                                        .unwrap_or(0);

                                                    // Extract if the device is removable (b -> bool)
                                                    let is_removable = block_props.get("HintRemovable")
                                                        .and_then(|v| bool::try_from(v.clone()).ok())
                                                        .unwrap_or(false);

                                                    // Extract hardware sector size property (u -> u32) dynamically from D-Bus
                                                    let sector_size = block_props.get("HwSectorSize")
                                                        .and_then(|v| u32::try_from(v.clone()).ok())
                                                        .map(|s| s as usize)
                                                        .unwrap_or(512); // Fallback if property is missing

                                                    // Extract human-readable label (s -> String), fallback to device filename
                                                    let name = block_props.get("IdLabel")
                                                        .and_then(|v| <String>::try_from(v.clone()).ok())
                                                        .filter(|s| !s.is_empty())
                                                        .unwrap_or_else(|| {
                                                            path.file_name()
                                                                .map(|n| n.to_string_lossy().into_owned())
                                                                .unwrap_or_else(|| "Generic Drive".to_string())
                                                        });

                                                    let device = BlockDevice {
                                                        path,
                                                        name,
                                                        size_bytes,
                                                        is_removable,
                                                        sector_size,
                                                    };

                                                    if tx.send(DeviceEvent::Added(device)).await.is_err() {
                                                        return; // Target receiver was dropped, terminate task
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // 2. Handle disconnected devices
                                    Some(removed) = removed_stream.next() => {
                                        let removed_args = removed.args().map_err(FlashError::from)?;
                                        if removed_args.interfaces.iter().any(|i| i == "org.freedesktop.UDisks2.Block") {
                                            // UDisks2 object paths match this pattern: "/org/freedesktop/UDisks2/block_devices/sdb"
                                            // Extract the last component ("sdb") to rebuild the system path ("/dev/sdb")
                                            let object_path_str = removed.object_path.as_str();
                                            if let Some(filename) = object_path_str.split('/').last() {
                                                let sys_path = PathBuf::from(format!("/dev/{}", filename));

                                                if tx.send(DeviceEvent::Removed(sys_path)).await.is_err() {
                                                    return; // Target receiver was dropped, terminate task
                                                }
                                            }
                                        }
                                    }

                                }
            }
        });

        Ok(ReceiverStream::new(rx))
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
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        rustix::io::pwrite(&self.file, buf, offset).map_err(std::io::Error::from)?;
        Ok(())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize> {
        let bytes_read =
            rustix::io::pread(&self.file, buf, offset).map_err(std::io::Error::from)?;
        Ok(bytes_read)
    }

    fn flush_to_disk(&mut self) -> FlashResult<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(self.size_bytes)
    }

    fn seek(&mut self, seek: SeekFrom) -> FlashResult<()> {
        self.file.seek(seek).map_err(FlashError::Io)?;
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
        let file = std::fs::File::from(std_fd);

        Ok(LinuxRawWriteHandle {
            file,
            sector_size: device.sector_size,
            size_bytes: device.size_bytes,
        })
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
