use crate::{
    data_types::BlockDevice,
    error::{
        FlashError,
        FlashResult,
    },
};
use serde::Deserialize;
use std::{
    io::{
        Seek,
        SeekFrom,
    },
    mem::size_of,
    os::windows::{
        fs::FileExt,
        io::{
            AsRawHandle,
            FromRawHandle,
        },
    },
    path::{
        Path,
        PathBuf,
    },
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle,
            HANDLE,
        },
        Storage::FileSystem::{
            CreateFileW,
            DeleteVolumeMountPointW,
            FILE_ATTRIBUTE_NORMAL,
            FILE_FLAGS_AND_ATTRIBUTES,
            FILE_GENERIC_READ,
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_SHARE_WRITE,
            FindFirstVolumeW,
            FindNextVolumeW,
            FindVolumeClose,
            FlushFileBuffers,
            GetVolumePathNamesForVolumeNameW,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                FSCTL_DISMOUNT_VOLUME,
                IOCTL_STORAGE_EJECT_MEDIA,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                STORAGE_DEVICE_NUMBER,
            },
        },
    },
    core::PCWSTR,
};
use wmi::{
    WMIConnection,
    WMIError,
};

use crate::traits::{
    DeviceEjector,
    DeviceEnumerator,
    DeviceUnmounter,
    DeviceWriter,
    RawWriteHandle,
};
#[derive(Deserialize)]
#[serde(rename = "Win32_DiskDrive")]
#[serde(rename_all = "PascalCase")]
struct Win32DiskDrive {
    #[serde(rename = "DeviceID")]
    device_id: String, // "\\.\PhysicalDrive0"
    model: String, // "Samsung USB Drive"
    size: Option<u64>,
    bytes_per_sector: u32,
    media_type: Option<String>, // "Removable Media" / "Fixed hard disk media"
}

/// Wraps [`HANDLE`] to make it [`Send`].
///
/// Windows HANDLEs for file/device objects are process-wide values that the
/// Win32 documentation explicitly permits to be used from any thread.
/// Safety is maintained by the `&mut self` receivers on [`RawWriteHandle`]:
/// at most one task can hold the handle at a time, so there is no concurrent
/// use across threads.
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct WindowsInterface;

#[allow(missing_docs)]
#[derive(Debug)]
pub struct WindowsRawWriteHandle {
    file: std::fs::File,
    sector_size: usize,
    size_bytes: u64,
}

impl RawWriteHandle for WindowsRawWriteHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        self.file
            .seek_write(buf, offset)
            .map_err(|_| FlashError::SyncError)?;

        Ok(())
    }

    /// Positional read via `ReadFile` + `OVERLAPPED` — mirror of `write_at`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize> {
        let bytes_read = self
            .file
            .seek_read(buf, offset)
            .map_err(|_| FlashError::SyncError)?;
        Ok(bytes_read)
    }

    /// Flush kernel write buffers to physical media via `FlushFileBuffers`.
    async fn flush_to_disk(&mut self) -> FlashResult<()> {
        let send_handle = SendHandle(HANDLE(self.file.as_raw_handle()));
        tokio::task::spawn_blocking(move || unsafe {
            let handle = send_handle;
            FlushFileBuffers(handle.0).map_err(FlashError::WindowsError)
        })
        .await
        .map_err(|_| FlashError::SyncError)??;
        Ok(())
    }

    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(self.size_bytes)
    }

    async fn seek(&mut self, seek: SeekFrom) -> FlashResult<()> {
        let mut fd = self.file.try_clone().map_err(|_| FlashError::SyncError)?;
        tokio::task::spawn_blocking(move || fd.seek(seek))
            .await
            .map_err(|_| FlashError::SyncError)??;
        Ok(())
    }
}

impl DeviceWriter for WindowsInterface {
    type Handle = WindowsRawWriteHandle;

    async fn open_for_writing(&self, device: &BlockDevice) -> FlashResult<Self::Handle> {
        let path = device.path.clone();
        let sector_size = device.sector_size;
        let size_bytes = device.size_bytes;

        tokio::task::spawn_blocking(move || -> FlashResult<WindowsRawWriteHandle> {
            let path_str = path.to_string_lossy().to_string();
            let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
                .map_err(FlashError::WindowsError)?
            };
            let file = unsafe { std::fs::File::from_raw_handle(handle.0 as _) };
            Ok(WindowsRawWriteHandle {
                file,
                sector_size,
                size_bytes,
            })
        })
        .await
        .map_err(|_| FlashError::SyncError)?
    }
}

impl DeviceEnumerator for WindowsInterface {
    async fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
        tokio::task::spawn_blocking(|| {
            let wmi = WMIConnection::new()
                .map_err(|e: WMIError| FlashError::FilesystemError(e.to_string()))?;

            let drives: Vec<Win32DiskDrive> = wmi
                .query()
                .map_err(|e: WMIError| FlashError::FilesystemError(e.to_string()))?;

            let devices = drives
                .into_iter()
                .map(|d| {
                    let size_bytes = d.size.unwrap_or(0);
                    let is_removable = d
                        .media_type
                        .as_deref()
                        .map(|m| m.contains("Removable"))
                        .unwrap_or(false);

                    BlockDevice::new(
                        PathBuf::from(&d.device_id),
                        d.model,
                        size_bytes,
                        is_removable,
                        d.bytes_per_sector as usize,
                    )
                })
                .collect();

            Ok(devices)
        })
        .await
        .map_err(|_| FlashError::SyncError)?
    }
}

impl DeviceEjector for WindowsInterface {
    async fn eject(&self, device: &BlockDevice) -> FlashResult<()> {
        let path = device.path.clone();
        tokio::task::spawn_blocking(move || -> FlashResult<()> {
            unmount_volumes_on_drive(&path)?;
            let path_str = path.to_string_lossy().to_string();
            let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
                .map_err(FlashError::WindowsError)?
            };

            unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_EJECT_MEDIA,
                    None,
                    0,
                    None,
                    0,
                    None,
                    None,
                )
                .ok();
                CloseHandle(handle).ok();
            }
            Ok(())
        })
        .await
        .map_err(|e| FlashError::FilesystemError(e.to_string()))?
    }
}

impl DeviceUnmounter for WindowsInterface {
    /// Dismount every volume on the target physical drive.
    ///
    /// This mirrors the `removeDriveLetters` + volume-dismount sequence from
    ///  1. Walk all system volumes via `FindFirstVolumeW` / `FindNextVolumeW`.
    ///  2. Identify which physical drive each volume lives on using
    ///     `IOCTL_STORAGE_GET_DEVICE_NUMBER`.
    ///  3. For matching volumes: remove mount-point paths with
    ///     `DeleteVolumeMountPointW`, then issue `FSCTL_DISMOUNT_VOLUME`.
    async fn unmount(&self, device: &BlockDevice) -> FlashResult<()> {
        let path = device.path.clone();
        tokio::task::spawn_blocking(move || unmount_volumes_on_drive(&path))
            .await
            .map_err(|_| FlashError::SyncError)?
    }
}

fn unmount_volumes_on_drive(physical_path: &Path) -> FlashResult<()> {
    let target_number = physical_drive_number(physical_path)?;

    let mut vol_buf = vec![0u16; 260];

    // FindFirstVolumeW fills vol_buf with the GUID path of the first volume,
    // e.g. "\\?\Volume{...}\".
    let find = unsafe {
        FindFirstVolumeW(&mut vol_buf)
            .map_err(|_| FlashError::FilesystemError("FindFirstVolumeW failed".into()))?
    };

    loop {
        let end = vol_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(vol_buf.len());
        let possible_string = &vol_buf.get(..end).ok_or(FlashError::OutOfBoundsArray)?;
        let guid_path = String::from_utf16_lossy(possible_string);

        // Strip the trailing '\' to form a device path accepted by CreateFileW.
        let device_path = guid_path.trim_end_matches('\\');
        let device_wide: Vec<u16> = device_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // Open with zero-access just to query the device number.
        if let Ok(vh) = unsafe {
            CreateFileW(
                PCWSTR(device_wide.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        } {
            let matches = storage_device_number(vh)
                .map(|n| n == target_number)
                .unwrap_or(false);
            unsafe { CloseHandle(vh).ok() };

            if matches {
                // Remove all mount points (drive letters / directory links).
                remove_mount_points(&guid_path);

                // Re-open with write access to issue FSCTL_DISMOUNT_VOLUME.
                if let Ok(wh) = unsafe {
                    CreateFileW(
                        PCWSTR(device_wide.as_ptr()),
                        (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    )
                } {
                    let mut returned = 0u32;
                    unsafe {
                        DeviceIoControl(
                            wh,
                            FSCTL_DISMOUNT_VOLUME,
                            None,
                            0,
                            None,
                            0,
                            Some(&mut returned),
                            None,
                        )
                        .ok();
                        CloseHandle(wh).ok();
                    }
                }
            }
        }

        // Advance; break when the enumeration is exhausted (ERROR_NO_MORE_FILES).
        vol_buf.fill(0);
        if unsafe { FindNextVolumeW(find, &mut vol_buf) }.is_err() {
            break;
        }
    }

    unsafe { FindVolumeClose(find).ok() };
    Ok(())
}

/// Parse the drive number from a path like `\\.\PhysicalDrive2` → `2`.
fn physical_drive_number(path: &Path) -> FlashResult<u32> {
    let s = path.to_string_lossy();
    let upper = s.to_ascii_uppercase();

    // Require the path to match exactly \\.\PHYSICALDRIVEn
    let prefix = r"\\.\PHYSICALDRIVE";
    let suffix = upper.strip_prefix(prefix).ok_or_else(|| {
        FlashError::FilesystemError(format!("path does not look like a physical drive: '{s}'"))
    })?;

    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return Err(FlashError::FilesystemError(format!(
            "invalid drive number in path: '{s}'"
        )));
    }

    suffix
        .parse()
        .map_err(|_| FlashError::FilesystemError(format!("cannot parse drive number from '{s}'")))
}

/// Return the physical device number for `handle` via
/// `IOCTL_STORAGE_GET_DEVICE_NUMBER`, or `None` on failure.
fn storage_device_number(handle: HANDLE) -> Option<u32> {
    let mut sdn = STORAGE_DEVICE_NUMBER::default();
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some(&mut sdn as *mut _ as *mut _),
            size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&mut returned),
            None,
        )
        .ok()?;
    }
    Some(sdn.DeviceNumber)
}

/// Remove every mount-point path (drive letter or directory junction)
/// associated with the volume whose GUID path is `guid_path`.
///
/// `guid_path` must end with `\`, as returned by `FindFirstVolumeW`.
fn remove_mount_points(guid_path: &str) {
    let guid_wide: Vec<u16> = guid_path.encode_utf16().chain(std::iter::once(0)).collect();
    // Allocate generously — `GetVolumePathNamesForVolumeNameW` may fail and
    // retry with a larger buffer, but 1 KiB covers typical cases.
    let mut buf = vec![0u16; 1024];
    let mut len = 0u32;

    if unsafe {
        GetVolumePathNamesForVolumeNameW(PCWSTR(guid_wide.as_ptr()), Some(&mut buf), &mut len)
    }
    .is_err()
    {
        return;
    }

    // The buffer is a multi-string: null-terminated strings back-to-back,
    // terminated by an extra null ("C:\\\0D:\\\0\0").
    let mut offset = 0usize;
    while offset < buf.len() {
        let term = buf[offset..].iter().position(|&c| c == 0).unwrap_or(0);
        if term == 0 {
            break; // double-null end marker
        }
        // Build a slice that includes the null terminator for PCWSTR.
        let mp = &buf[offset..offset + term + 1];
        unsafe {
            DeleteVolumeMountPointW(PCWSTR(mp.as_ptr())).ok();
        }
        offset += term + 1;
    }
}
