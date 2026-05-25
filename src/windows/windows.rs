use crate::{
    data_types::BlockDevice,
    error::{FlashError, FlashResult},
};
use std::{
    io::SeekFrom,
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            DIGCF_PRESENT, SP_DEVINFO_DATA, SPDRP_FRIENDLYNAME, SetupDiDestroyDeviceInfoList,
            SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW,
        },
        Foundation::{CloseHandle, HANDLE},
        Storage::FileSystem::{
            CreateFileW, DeleteVolumeMountPointW, FILE_BEGIN, FILE_CURRENT, FILE_END,
            FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, FlushFileBuffers,
            GetVolumePathNamesForVolumeNameW, OPEN_EXISTING, ReadFile, SetFilePointerEx, WriteFile,
        },
        System::{
            IO::{DeviceIoControl, OVERLAPPED},
            Ioctl::{
                DISK_GEOMETRY_EX, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_EJECT_MEDIA,
                IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
                PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
                STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
            },
        },
    },
    core::{GUID, PCWSTR},
};

use crate::traits::{
    DeviceEjector, DeviceEnumerator, DeviceUnmounter, DeviceWriter, RawWriteHandle,
};

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
    /// Positional write via `WriteFile` + `OVERLAPPED`.
    ///
    /// Even on handles NOT opened with `FILE_FLAG_OVERLAPPED`, Windows
    /// honours the `Offset`/`OffsetHigh` fields of an `OVERLAPPED` struct
    /// to select the write position, completing the call synchronously.
    /// This is the Windows equivalent of `pwrite(2)`.
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        // Wrap in SendHandle before moving into spawn_blocking so the closure
        // is `'static + Send`.  All other methods below follow the same pattern.
        let send_handle = SendHandle(HANDLE(self.file.as_raw_handle()));
        let ptr = buf.as_ptr() as usize;
        let len = buf.len();

        tokio::task::spawn_blocking(move || {
            let handle = send_handle;
            // Safety: `ptr`/`len` describe a slice owned by the caller that
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };

            let mut overlapped = OVERLAPPED::default();

            overlapped.Anonymous.Anonymous.Offset = offset as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;

            let mut bytes_written = 0u32;
            unsafe {
                WriteFile(
                    handle.0,
                    Some(slice),
                    Some(&mut bytes_written),
                    Some(&mut overlapped),
                )
                .map_err(FlashError::WindowsError)
            }
        })
        .await
        .map_err(|_| FlashError::SyncError)??;
        Ok(())
    }

    /// Positional read via `ReadFile` + `OVERLAPPED` — mirror of `write_at`.
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize> {
        let send_handle = SendHandle(HANDLE(self.file.as_raw_handle()));
        let ptr = buf.as_mut_ptr() as usize;
        let len = buf.len();

        let bytes_read = tokio::task::spawn_blocking(move || {
            let handle = send_handle;
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };

            let mut overlapped = OVERLAPPED::default();
            overlapped.Anonymous.Anonymous.Offset = offset as u32;
            overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;

            let mut bytes_read = 0u32;
            unsafe {
                ReadFile(
                    handle.0,
                    Some(slice),
                    Some(&mut bytes_read),
                    Some(&mut overlapped),
                )
                .map_err(FlashError::WindowsError)?;
            }
            Ok::<usize, FlashError>(bytes_read as usize)
        })
        .await
        .map_err(|_| FlashError::SyncError)??;
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
        let send_handle = SendHandle(HANDLE(self.file.as_raw_handle()));
        tokio::task::spawn_blocking(move || {
            let handle = send_handle;
            let (method, dist) = match seek {
                SeekFrom::Start(n) => (FILE_BEGIN, n as i64),
                SeekFrom::Current(n) => (FILE_CURRENT, n),
                SeekFrom::End(n) => (FILE_END, n),
            };
            unsafe {
                SetFilePointerEx(handle.0, dist, None, method).map_err(FlashError::WindowsError)
            }
        })
        .await
        .map_err(|_| FlashError::SyncError)??;
        Ok(())
    }
}

impl DeviceWriter for WindowsInterface {
    type Handle = WindowsRawWriteHandle;

    /// Open a physical drive for raw writing.
    ///
    /// After opening, we attempt `FSCTL_LOCK_VOLUME` in a retry loop (up to
    /// 10 attempts × 500 ms) so that Windows releases any buffered filesystem
    /// `WinDiskManagement::lockDrive`.  The lock is held for the lifetime of
    /// the returned handle; closing the handle releases it automatically.
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
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    None,
                )
                .map_err(FlashError::WindowsError)?
            };

            // Retry FSCTL_LOCK_VOLUME so transient filesystem activity
            // (e.g. Explorer thumbnailing) doesn't cause an immediate failure.
            let mut locked = false;
            for _ in 0..10 {
                let mut returned = 0u32;
                if unsafe {
                    DeviceIoControl(
                        handle,
                        FSCTL_LOCK_VOLUME,
                        None,
                        0,
                        None,
                        0,
                        Some(&mut returned),
                        None,
                    )
                }
                .is_ok()
                {
                    locked = true;
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }

            if !locked {
                unsafe { CloseHandle(handle).ok() };
                return Err(FlashError::DeviceBusy { path });
            }

            // Transfer ownership to std::fs::File.  Its Drop impl calls
            // CloseHandle, which also releases the FSCTL_LOCK_VOLUME lock.
            let file = unsafe { std::fs::File::from_raw_handle(handle.0) };
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
            let mut devices = Vec::new();

            for device_number in 0u32..16 {
                let path_str = format!(r"\\.\PhysicalDrive{}", device_number);
                let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

                let handle = unsafe {
                    CreateFileW(
                        PCWSTR(wide.as_ptr()),
                        0, // zero access — enough to issue IOCTLs
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    )
                };

                let handle = match handle {
                    Ok(h) if h.is_invalid() => h,
                    _ => continue,
                };

                let mut geo = DISK_GEOMETRY_EX::default();
                let mut returned = 0u32;

                let geo_ok = unsafe {
                    DeviceIoControl(
                        handle,
                        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                        None,
                        0,
                        Some(&mut geo as *mut _ as *mut _),
                        size_of::<DISK_GEOMETRY_EX>() as u32,
                        Some(&mut returned),
                        None,
                    )
                };

                unsafe { CloseHandle(handle).ok() };

                if geo_ok.is_err() {
                    continue;
                }

                let size_bytes = geo.DiskSize as u64;
                let sector_size = geo.Geometry.BytesPerSector as usize;
                let is_removable = query_removable(&path_str).unwrap_or(false);
                let name = get_friendly_name(device_number)
                    .unwrap_or_else(|| format!("Physical Drive {}", device_number));

                devices.push(BlockDevice::new(
                    PathBuf::from(&path_str),
                    name,
                    size_bytes,
                    is_removable,
                    sector_size,
                ));
            }

            Ok(devices)
        })
        .await
        .map_err(|_| FlashError::DeviceBusy {
            path: PathBuf::new(),
        })?
    }
}

impl DeviceEjector for WindowsInterface {
    async fn eject(&self, device: &BlockDevice) -> FlashResult<()> {
        let path = device.path.clone();
        tokio::task::spawn_blocking(move || -> FlashResult<()> {
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

            let mut returned = 0u32;
            unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_EJECT_MEDIA,
                    None,
                    0,
                    None,
                    0,
                    Some(&mut returned),
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

// ── DeviceUnmounter ───────────────────────────────────────────────────────────

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
    s.rsplit("PhysicalDrive")
        .next()
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| FlashError::FilesystemError(format!("cannot parse drive number from '{s}'")))
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

fn query_removable(path: &str) -> Option<bool> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .ok()?
    };

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };

    let mut buf = vec![0u8; 1024];
    let mut returned = 0u32;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const _),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
    };

    unsafe { CloseHandle(handle).ok() };

    if ok.is_err() {
        return None;
    }

    // Safety: the IOCTL filled at least sizeof(STORAGE_DEVICE_DESCRIPTOR) bytes
    // into `buf`, which is large enough (1 KiB).
    let desc = unsafe { &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    // In windows-crate 0.62, RemovableMedia is already `bool`.
    Some(desc.RemovableMedia)
}

fn get_friendly_name(drive_index: u32) -> Option<String> {
    // Disk class GUID: {4D36E967-E325-11CE-BFC1-08002BE10318}
    let disk_guid = GUID::from_values(
        0x4D36E967,
        0xE325,
        0x11CE,
        [0xBF, 0xC1, 0x08, 0x00, 0x2B, 0xE1, 0x03, 0x18],
    );

    let set = unsafe { SetupDiGetClassDevsW(Some(&disk_guid), None, None, DIGCF_PRESENT).ok()? };

    let mut dev_info = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };

    // Enumerate device at `drive_index`.  On failure always destroy the set.
    if unsafe { SetupDiEnumDeviceInfo(set, drive_index, &mut dev_info) }.is_err() {
        unsafe { SetupDiDestroyDeviceInfoList(set).ok() };
        return None;
    }

    // The property value is stored as UTF-16 bytes.  The windows-crate 0.62
    // API takes `Option<&mut [u8]>` (buffer length is inferred from the slice
    // length), replacing the old separate `cbPropertyBufferSize: u32` argument.
    let mut buf = vec![0u8; 512]; // 256 UTF-16 code units × 2 bytes each

    let prop_result = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            &dev_info,
            SPDRP_FRIENDLYNAME,
            None, // don't need the registry type tag
            Some(&mut buf),
            None, // don't need the required-size hint
        )
    };

    // Always destroy the device-info set, even if the property query failed.
    unsafe { SetupDiDestroyDeviceInfoList(set).ok() };

    prop_result.ok()?;

    // Reinterpret the raw bytes as a UTF-16 string and strip the null terminator.
    // Safety: `buf` is aligned to 1 byte; `from_raw_parts` with `*const u16`
    // requires only that the pointer is valid for `len` u16 reads, which it is.
    let u16_slice =
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u16, buf.len() / 2) };
    let end = u16_slice
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(u16_slice.len());
    Some(String::from_utf16_lossy(&u16_slice[..end]))
}
