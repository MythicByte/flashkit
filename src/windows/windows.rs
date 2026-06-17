//! see for more information about dismount [dismount](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_dismount_volume)
use crate::{
    data_types::{
        BlockDevice,
        DeviceEvent,
    },
    error::{
        FlashError,
        FlashResult,
    },
    traits::AsyncDeviceEnumerator,
};
use std::{
    collections::HashSet,
    ffi::c_void,
    io::{
        Seek,
        SeekFrom,
    },
    iter::once,
    mem::size_of,
    os::windows::{
        fs::{
            FileExt,
            OpenOptionsExt,
        },
        io::AsRawHandle,
    },
    path::{
        Path,
        PathBuf,
    },
    thread,
    time::Duration,
};
use tokio_stream::wrappers::ReceiverStream;
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_REMOVAL_POLICY,
            CM_REMOVAL_POLICY_EXPECT_ORDERLY_REMOVAL,
            CM_REMOVAL_POLICY_EXPECT_SURPRISE_REMOVAL,
            DIGCF_DEVICEINTERFACE,
            DIGCF_PRESENT,
            SP_DEVICE_INTERFACE_DATA,
            SP_DEVICE_INTERFACE_DETAIL_DATA_W,
            SP_DEVINFO_DATA,
            SPDRP_FRIENDLYNAME,
            SPDRP_REMOVAL_POLICY,
            SetupDiDestroyDeviceInfoList,
            SetupDiEnumDeviceInfo,
            SetupDiEnumDeviceInterfaces,
            SetupDiGetClassDevsW,
            SetupDiGetDeviceInterfaceDetailW,
            SetupDiGetDeviceRegistryPropertyW,
        },
        Foundation::{
            CloseHandle,
            HANDLE,
        },
        Storage::FileSystem::{
            CreateFileW,
            DeleteVolumeMountPointW,
            FILE_ATTRIBUTE_NORMAL,
            FILE_FLAGS_AND_ATTRIBUTES,
            FILE_SHARE_READ,
            FILE_SHARE_WRITE,
            FindFirstVolumeW,
            FindNextVolumeW,
            FindVolumeClose,
            GetDriveTypeW,
            GetLogicalDrives,
            GetVolumePathNamesForVolumeNameW,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                DISK_GEOMETRY_EX,
                FSCTL_DISMOUNT_VOLUME,
                FSCTL_LOCK_VOLUME,
                GUID_DEVINTERFACE_DISK,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                IOCTL_STORAGE_EJECT_MEDIA,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                STORAGE_DEVICE_NUMBER,
                VOLUME_DISK_EXTENTS,
            },
        },
    },
    core::PCWSTR,
};

use crate::traits::{
    DeviceEjector,
    DeviceEnumerator,
    DeviceUnmounter,
    DeviceWriter,
    RawWriteHandle,
};

/// Closes any file or device HANDLE on drop.
#[derive(Debug)]
struct AutoCloseHandle(HANDLE);

impl Drop for AutoCloseHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                CloseHandle(self.0).ok();
            }
        }
    }
}
// SAFETY: Windows HANDLEs are valid to send across threads and access from
// multiple threads.  The OS itself is responsible for synchronising access
// to the underlying kernel object.
unsafe impl Send for AutoCloseHandle {}
unsafe impl Sync for AutoCloseHandle {}

/// Owns a `FindFirstVolumeW` enumeration cursor and closes it on drop.
///
/// The raw HANDLE is kept private, all access goes through the typed methods
/// below.  This prevents the Copy-able raw value from *escaping* and being
/// used independently of its owner.
#[derive(Debug)]
struct VolumeFindHandle(HANDLE);

impl VolumeFindHandle {
    /// Begin volume enumeration.  Writes the first volume's GUID path into
    /// `buf` and returns the guard that owns the cursor.
    ///
    /// A volume GUID path looks like `\\?\Volume{xxxxxxxx-...}\` ,
    /// letter-independent name for a volume that does not change even if the
    /// drive letter is reassigned.
    fn start(buf: &mut [u16]) -> FlashResult<Self> {
        let handle = unsafe {
            FindFirstVolumeW(buf)
                .map_err(|_| FlashError::FilesystemError("FindFirstVolumeW failed".into()))?
        };
        // INVALID_HANDLE_VALUE here would mean the system has no volumes at
        // all — not possible in practice, but we guard against it anyway.
        if handle.is_invalid() {
            return Err(FlashError::SyncError);
        }
        Ok(Self(handle))
    }

    /// Advance the cursor and write the next GUID path into `buf`.
    /// Returns `true` while more volumes remain, `false` when the list is
    /// exhausted (Windows signals ERROR_NO_MORE_FILES).
    fn advance(&self, buf: &mut [u16]) -> bool {
        unsafe { FindNextVolumeW(self.0, buf) }.is_ok()
    }
}

impl Drop for VolumeFindHandle {
    fn drop(&mut self) {
        unsafe { FindVolumeClose(self.0).ok() };
    }
}

#[allow(missing_docs)]
#[derive(Debug, Default)]
pub struct WindowsRawWriteHandle {
    file: Option<std::fs::File>,
    sector_size: usize,
    size_bytes: u64,
    /// Keeps the FSCTL_LOCK_VOLUME handles alive for every volume on this
    /// physical drive.  Dropping them releases the locks and lets Windows
    /// remount the filesystems, so they must outlive every write/flush.
    _volume_locks: Vec<std::fs::File>,
}

impl RawWriteHandle for WindowsRawWriteHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        match &mut self.file {
            Some(file) => {
                file.seek_write(buf, offset)
                    .map_err(|e| FlashError::Io(e))?;
            }
            None => return Err(FlashError::WindowsHandle),
        }

        Ok(())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize> {
        let bytes_read = match &mut self.file {
            Some(file) => file.seek_read(buf, offset).map_err(|e| FlashError::Io(e))?,
            None => return Err(FlashError::WindowsHandle),
        };
        Ok(bytes_read)
    }

    /// Flush kernel write buffers to physical media
    fn flush_to_disk(&mut self) -> FlashResult<()> {
        match &mut self.file {
            Some(file) => {
                file.sync_all()?;
            }
            None => return Err(FlashError::WindowsHandle),
        }
        Ok(())
    }

    fn sector_size(&self) -> usize {
        self.sector_size
    }

    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(self.size_bytes)
    }

    fn seek(&mut self, seek: SeekFrom) -> FlashResult<()> {
        match &mut self.file {
            Some(file) => {
                file.seek(seek).map_err(|e| FlashError::Io(e))?;
            }
            None => return Err(FlashError::WindowsHandle),
        }
        Ok(())
    }
}

impl DeviceWriter for WindowsRawWriteHandle {
    type Handle = WindowsRawWriteHandle;

    // Inside `impl DeviceWriter for WindowsRawWriteHandle`
    async fn open_for_writing(&self, device: &BlockDevice) -> FlashResult<Self::Handle> {
        let path = device.path.clone();
        let sector_size = device.sector_size;
        let size_bytes = device.size_bytes;
        let path_str = path.to_string_lossy().to_string();

        // Lock and dismount all volumes
        let volume_locks = unmount_volumes_on_drive_locked(&path)?;

        let file = 'attempt: {
            let mut last_error = FlashError::WindowsHandle;

            for _ in 0..10 {
                let file_result = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
                    .open(&path_str);

                match file_result {
                    Ok(f) => break 'attempt Ok(f),
                    Err(e) => last_error = FlashError::Io(e),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(last_error)
        }?;

        Ok(WindowsRawWriteHandle {
            file: Some(file),
            sector_size,
            size_bytes,
            _volume_locks: volume_locks,
        })
    }
}

impl DeviceEnumerator for WindowsRawWriteHandle {
    async fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
        Self::list_devices().await
    }
}

impl DeviceEjector for WindowsRawWriteHandle {
    /// eject via **DeviceIoControl**
    async fn eject(&self, _device: &BlockDevice) -> FlashResult<()> {
        if let Some(file) = &self.file {
            let handle = HANDLE(file.as_raw_handle());
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
                .map_err(FlashError::WindowsError)?;
            }
            return Ok(());
        }
        Err(FlashError::WindowsGenericError)
    }
}

impl DeviceUnmounter for WindowsRawWriteHandle {
    /// removed because of complex configureguration moved into [open_for_writing]
    ///
    /// **does nothing**
    ///
    /// fixes a problem with the traits and no need to rewrite flasher. Would need to be rewritten
    async fn unmount(&self, _device: &BlockDevice) -> FlashResult<()> {
        Ok(())
    }
}

/// Dismount every volume on `physical_path` and **return the lock handles**.
///
/// The caller must keep the returned `Vec<AutoCloseHandle>` alive for the
/// entire flash operation.  Dropping them releases `FSCTL_LOCK_VOLUME` and
/// lets Windows remount the filesystems, corrupting mid-flash writes.
fn unmount_volumes_on_drive_locked(physical_path: &Path) -> FlashResult<Vec<std::fs::File>> {
    const VOLUME_GUID_BUF_LEN: usize = 64;
    let mut vol_buf = vec![0u16; VOLUME_GUID_BUF_LEN];

    let target_number = physical_drive_number(physical_path)?;

    // The guard calls `FindVolumeClose` when it drops.
    let finder = VolumeFindHandle::start(&mut vol_buf)?;
    let mut locks: Vec<std::fs::File> = Vec::new();

    loop {
        let guid_path = decode_wide_nul_string(&vol_buf);

        // Returns the lock handle for matching volumes; None = different drive.
        if let Some(lock) = process_single_volume(&guid_path, target_number)? {
            locks.push(lock);
        }

        vol_buf.fill(0);
        if !finder.advance(&mut vol_buf) {
            break;
        }
    }

    Ok(locks)
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
    let mut returned = 0u32;
    #[repr(C)]
    struct DISK_EXTENT {
        disk_number: u32,
        starting_offset: i64,
        extent_length: i64,
    }
    #[repr(C)]
    struct VOLUME_DISK_EXTENTS {
        number_of_disk_extents: u32,
        extents: [DISK_EXTENT; 1],
    }

    let mut extents = VOLUME_DISK_EXTENTS {
        number_of_disk_extents: 0,
        extents: [DISK_EXTENT {
            disk_number: 0,
            starting_offset: 0,
            extent_length: 0,
        }],
    };

    let success_extents = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            0,
            Some(&mut extents as *mut _ as *mut _),
            size_of::<VOLUME_DISK_EXTENTS>() as u32,
            Some(&mut returned),
            None,
        )
        .is_ok()
    };

    if success_extents && returned > 0 && extents.number_of_disk_extents == 1 {
        return Some(extents.extents[0].disk_number);
    }

    //  Fallback to IOCTL_STORAGE_GET_DEVICE_NUMBER
    let mut sdn = STORAGE_DEVICE_NUMBER::default();
    let success_sdn = unsafe {
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
        .is_ok()
    };

    if success_sdn {
        Some(sdn.DeviceNumber)
    } else {
        None
    }
}

/// `GetVolumePathNamesForVolumeNameW` returns all of a volume's mount points
/// as a "multi-string": consecutive null-terminated UTF-16 strings packed into
/// one buffer, ending with an extra null:
///
///   C:\<NUL>D:\Data\<NUL><NUL>
///
/// The safe approach is the two-pass pattern:
///   Pass 1 — call with a 1-element probe buffer.  The call fails but Windows
///             sets `required_chars` to the number of UTF-16 code units needed.
///   Pass 2 — allocate exactly that many code units and call again.
///
/// A fixed-size buffer (as the original used) silently fails when a drive has
/// many or long mount points, leaving them attached and causing the later
/// FSCTL_DISMOUNT_VOLUME to produce incomplete results.

fn remove_mount_points(guid_path: &str) {
    let guid_wide: Vec<u16> = guid_path.encode_utf16().chain(once(0)).collect();

    let mut required_chars = 0u32;
    let mut probe = [0u16; 1];
    let _ = unsafe {
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(guid_wide.as_ptr()),
            Some(&mut probe),
            &mut required_chars,
        )
    };

    if required_chars == 0 {
        // no mount points, or volume not accessible
        return;
    }

    let mut buf = vec![0u16; required_chars as usize];
    if unsafe {
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(guid_wide.as_ptr()),
            Some(&mut buf),
            &mut required_chars,
        )
    }
    .is_err()
    {
        // Should not happen after a correctly-sized allocation, but we stop
        // rather than walk uninitialised memory.
        return;
    }

    // Walk the multi-string and delete each mount point
    //
    //   C:\<NUL>D:\Data\<NUL><NUL>
    //   ^-------^              ^^
    //   first entry       double-null = end of list
    let mut offset = 0usize;
    loop {
        let term = buf[offset..].iter().position(|&c| c == 0).unwrap_or(0);
        if term == 0 {
            break; // double-null: no more entries
        }
        // Include the null terminator — PCWSTR is a C-style null-terminated pointer.
        let mount_point = &buf[offset..offset + term + 1];
        unsafe {
            DeleteVolumeMountPointW(PCWSTR(mount_point.as_ptr())).ok();
        }
        offset += term + 1;
    }
}
/// Decode a null-terminated UTF-16 string from a buffer.
///
/// `position` returns an index guaranteed to be ≤ `buf.len()`, so the slice
/// is always in-bounds — no fallible `get` or `?` needed.
fn decode_wide_nul_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn process_single_volume(
    guid_path: &str,
    target_number: u32,
) -> FlashResult<Option<std::fs::File>> {
    let device_path = guid_path.trim_end_matches('\\');

    //  Query Handle
    let query_file = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .open(device_path)
        .map_err(|e| {
            FlashError::FilesystemError(format!("Failed to query volume attributes: {e}"))
        })?;

    // Safely cast to HANDLE for DeviceIoControl
    let raw_query_handle = HANDLE(query_file.as_raw_handle() as _);
    let is_match = storage_device_number(raw_query_handle) == Some(target_number);

    drop(query_file); // Automatically closes the handle

    if !is_match {
        return Ok(None);
    }

    //  Write Handle
    let write_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .open(device_path)
        .map_err(|e| FlashError::FilesystemError(format!("Access denied opening volume: {e}")))?;

    let raw_write_handle = HANDLE(write_file.as_raw_handle() as _);

    let mut locked = false;
    for _ in 0..10 {
        if unsafe {
            DeviceIoControl(
                raw_write_handle,
                FSCTL_LOCK_VOLUME,
                None,
                0,
                None,
                0,
                None,
                None,
            )
            .is_ok()
        } {
            locked = true;
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }

    if !locked {
        return Err(FlashError::FilesystemError("Failed to lock volume".into()));
    }

    unsafe {
        DeviceIoControl(
            raw_write_handle,
            FSCTL_DISMOUNT_VOLUME,
            None,
            0,
            None,
            0,
            None,
            None,
        )
    }
    .map_err(|e| FlashError::FilesystemError(format!("Failed to dismount volume: {e}")))?;

    remove_mount_points(guid_path);

    // Return the std::fs::File so it stays alive and keeps the lock
    Ok(Some(write_file))
}
/// Query the system for all mount points (drive letters or paths)
/// associated with a specific physical drive number.
/// for getting the letter name
pub fn get_drive_letters_for_drive(device: BlockDevice) -> Vec<String> {
    let mut letters = Vec::new();

    //  Safely extract the drive number  from "\\.\PhysicalDrive2"
    let target_number = match physical_drive_number(&device.path) {
        Ok(num) => num,
        Err(_) => return letters, // Return empty if path can't be parsed
    };
    const VOLUME_GUID_BUF_LEN: usize = 64;
    let mut vol_buf = vec![0u16; VOLUME_GUID_BUF_LEN];

    // Reuse your custom VolumeFindHandle logic
    let finder = match VolumeFindHandle::start(&mut vol_buf) {
        Ok(f) => f,
        Err(_) => return letters,
    };

    loop {
        let guid_path = decode_wide_nul_string(&vol_buf);

        if let Some(vol_letters) = get_single_volume_letters(&guid_path, target_number) {
            letters.extend(vol_letters);
        }

        vol_buf.fill(0);
        if !finder.advance(&mut vol_buf) {
            break;
        }
    }

    letters
}

/// Helper that checks if a volume belongs to the target drive, and collects its paths.
fn get_single_volume_letters(guid_path: &str, target_number: u32) -> Option<Vec<String>> {
    let device_path = guid_path.trim_end_matches('\\');
    let device_wide: Vec<u16> = device_path.encode_utf16().chain(once(0)).collect();

    // Open handle with zero desired access to safely inspect the device number
    let query_handle = unsafe {
        CreateFileW(
            PCWSTR(device_wide.as_ptr()),
            0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .ok()?
    };
    let query_guard = AutoCloseHandle(query_handle);

    // If it doesn't match our drive, skip it
    if storage_device_number(query_handle) != Some(target_number) {
        return None;
    }
    drop(query_guard); // Close handle early before querying path names

    // Two-pass approach to safely retrieve the mount points buffer
    let guid_wide: Vec<u16> = guid_path.encode_utf16().chain(once(0)).collect();
    let mut required_chars = 0u32;
    let mut probe = [0u16; 1];
    let _ = unsafe {
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(guid_wide.as_ptr()),
            Some(&mut probe),
            &mut required_chars,
        )
    };

    if required_chars == 0 {
        return Some(Vec::new());
    }

    let mut buf = vec![0u16; required_chars as usize];
    if unsafe {
        GetVolumePathNamesForVolumeNameW(
            PCWSTR(guid_wide.as_ptr()),
            Some(&mut buf),
            &mut required_chars,
        )
    }
    .is_err()
    {
        return Some(Vec::new());
    }

    // Parse the multi-string buffer into separate String elements
    let mut vol_letters = Vec::new();
    let mut offset = 0usize;
    loop {
        let term = buf[offset..].iter().position(|&c| c == 0).unwrap_or(0);
        if term == 0 {
            // Double-null terminator reached
            break;
        }

        let mount_point = &buf[offset..offset + term];
        let path_str = String::from_utf16_lossy(mount_point);
        if !path_str.is_empty() {
            vol_letters.push(path_str);
        }
        offset += term + 1;
    }

    Some(vol_letters)
}
impl AsyncDeviceEnumerator for WindowsRawWriteHandle {
    type WatchStream = ReceiverStream<DeviceEvent>;

    async fn watch_devices(&self) -> FlashResult<Self::WatchStream> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        // Fetch current devices and send them immediately
        let initial_devices = self.list_devices().await?;

        for dev in initial_devices.clone() {
            if tx.send(DeviceEvent::Added(dev)).await.is_err() {
                return Ok(ReceiverStream::new(rx));
            }
        }

        tokio::spawn(async move {
            let mut known_paths: HashSet<PathBuf> =
                initial_devices.into_iter().map(|d| d.path).collect();

            // Polling loop replacing WMI
            loop {
                // Adjust the polling interval to your preference.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

                // Re-scan active drives
                let current_devices = match Self::list_devices().await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("Failed to re-enumerate devices during polling: {e}");
                        continue;
                    }
                };

                let current_paths: HashSet<PathBuf> =
                    current_devices.iter().map(|d| d.path.clone()).collect();

                // Check for new devices
                for dev in current_devices {
                    if !known_paths.contains(&dev.path) {
                        known_paths.insert(dev.path.clone());
                        if tx.send(DeviceEvent::Added(dev)).await.is_err() {
                            return;
                        }
                    }
                }

                // Check for removed devices
                let removed_paths: Vec<PathBuf> =
                    known_paths.difference(&current_paths).cloned().collect();
                for path in removed_paths {
                    known_paths.remove(&path);
                    if tx.send(DeviceEvent::Removed(path)).await.is_err() {
                        return; // Receiver was dropped, exit task cleanly
                    }
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}
impl WindowsRawWriteHandle {
    /// for that the trait can not be used, should be cleanup later
    async fn list_devices() -> FlashResult<Vec<BlockDevice>> {
        let mut output = Vec::new();
        let devices = unsafe {
            // get all devices
            SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_DISK),
                None,
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(FlashError::WindowsError)?;
        // check handle
        if devices.is_invalid() {
            return Err(FlashError::WindowsGenericError);
        }
        let mut index_get_device_number = 0;
        // the cbSize is needed from the docs [check]: https://learn.microsoft.com/de-de/windows/win32/api/setupapi/nf-setupapi-setupdienumdeviceinfo
        let mut info_holder = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        while let Ok(_) =
            unsafe { SetupDiEnumDeviceInfo(devices, index_get_device_number, &mut info_holder) }
        {
            // for next device
            index_get_device_number += 1;

            let is_removable = {
                let mut status_give = CM_REMOVAL_POLICY(0u32);
                let check = unsafe {
                    SetupDiGetDeviceRegistryPropertyW(
                        devices,
                        &info_holder,
                        SPDRP_REMOVAL_POLICY,
                        None,
                        Some(std::slice::from_raw_parts_mut(
                            &mut status_give as *mut CM_REMOVAL_POLICY as *mut u8, // cast newtype ptr
                            std::mem::size_of::<CM_REMOVAL_POLICY>(),
                        )),
                        None,
                    )
                };
                match check {
                    Ok(_) => matches!(
                        status_give,
                        CM_REMOVAL_POLICY_EXPECT_ORDERLY_REMOVAL
                            | CM_REMOVAL_POLICY_EXPECT_SURPRISE_REMOVAL
                    ),
                    Err(_) => false,
                }
            };
            let name = {
                let mut buffer = [0u16; 260];
                let check = unsafe {
                    SetupDiGetDeviceRegistryPropertyW(
                        devices,
                        &info_holder,
                        SPDRP_FRIENDLYNAME,
                        None,
                        Some(std::slice::from_raw_parts_mut(
                            // reinterpret u16 buffer as &mut [u8]
                            buffer.as_mut_ptr() as *mut u8,
                            std::mem::size_of_val(&buffer), // 260 * 2 = 520 bytes
                        )),
                        None,
                    )
                };
                match check {
                    Ok(_) => {
                        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
                        String::from_utf16_lossy(&buffer.get(..end).unwrap_or_default())
                    }
                    Err(_) => String::new(),
                }
            };
            if name.is_empty() {
                continue;
            }
            let display_path = "Holder".to_string();
            let mut interface_data = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if unsafe {
                SetupDiEnumDeviceInterfaces(
                    devices,
                    Some(&info_holder),
                    &GUID_DEVINTERFACE_DISK,
                    0, // member index — first (and usually only) disk interface
                    &mut interface_data,
                )
            }
            .is_err()
            {
                tracing::debug!(
                    "device '{}' ({}): no GUID_DEVINTERFACE_DISK interface, skipping",
                    display_path,
                    name
                );
                continue;
            }

            let device_wide: Vec<u16> = {
                let mut required_size = 0u32;
                // Windows fills required_size with the bytes needed
                let _ = unsafe {
                    SetupDiGetDeviceInterfaceDetailW(
                        devices,
                        &interface_data,
                        None,
                        0,
                        Some(&mut required_size),
                        None,
                    )
                };
                let min_size = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                if required_size < min_size {
                    tracing::warn!(
                        "device '{}' ({}): detail required_size={} < min={}, skipping",
                        display_path,
                        name,
                        required_size,
                        min_size
                    );
                    continue;
                }
                let mut buf: Vec<u8> = vec![0u8; required_size as usize];
                let detail_ptr = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                unsafe {
                    (*detail_ptr).cbSize = min_size;
                }
                if unsafe {
                    SetupDiGetDeviceInterfaceDetailW(
                        devices,
                        &interface_data,
                        Some(&mut *detail_ptr),
                        required_size,
                        None,
                        None,
                    )
                }
                .is_err()
                {
                    tracing::warn!(
                        "device '{}' ({}): SetupDiGetDeviceInterfaceDetailW pass 2 failed",
                        display_path,
                        name
                    );
                    continue;
                }
                // The device path lives immediately after cbSize (4 bytes)
                let path_offset = std::mem::size_of::<u32>();
                let path_u16: &[u16] = unsafe {
                    std::slice::from_raw_parts(
                        buf[path_offset..].as_ptr() as *const u16,
                        (required_size as usize - path_offset) / 2,
                    )
                };
                let nul = path_u16
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(path_u16.len());
                path_u16[..=nul].to_vec() // include null terminator for PCWSTR
            };

            let (size_bytes, sector_size, opened_device) = {
                let mut info_back: u32 = 0;
                let mut geometry = DISK_GEOMETRY_EX::default();
                let out_buffer_size = std::mem::size_of::<DISK_GEOMETRY_EX>() as u32;

                let opened_device = match unsafe {
                    CreateFileW(
                        windows::core::PCWSTR(device_wide.as_ptr()),
                        0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL,
                        None,
                    )
                } {
                    Ok(h) => AutoCloseHandle(h),
                    Err(e) => {
                        let path_str = String::from_utf16_lossy(
                            device_wide.split(|&c| c == 0).next().unwrap_or(&[]),
                        );
                        tracing::warn!(
                            "device '{}' ({}): CreateFileW('{}') – {} – skipping",
                            display_path,
                            name,
                            path_str,
                            e
                        );
                        continue;
                    }
                };
                let _ = unsafe {
                    DeviceIoControl(
                        opened_device.0,
                        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                        None,
                        0,
                        Some((&mut geometry) as *mut _ as *mut c_void),
                        out_buffer_size,
                        Some(&mut info_back),
                        None,
                    )
                };
                (
                    geometry.DiskSize as u64,
                    geometry.Geometry.BytesPerSector as usize,
                    opened_device,
                )
            };

            let (path, disk_number) = {
                let mut disk_number: i32 = -1;
                let mut size: u32 = 0;

                let mut disk_extents = VOLUME_DISK_EXTENTS::default();
                let res1 = unsafe {
                    DeviceIoControl(
                        opened_device.0,
                        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                        None,
                        0,
                        Some(&mut disk_extents as *mut VOLUME_DISK_EXTENTS as *mut _),
                        std::mem::size_of::<VOLUME_DISK_EXTENTS>() as u32,
                        Some(&mut size),
                        None,
                    )
                };

                if res1.is_ok() && disk_extents.NumberOfDiskExtents > 0 {
                    // Ignore RAIDs if there are 2 or more extents
                    if disk_extents.NumberOfDiskExtents >= 2 {
                        disk_number = -1;
                    } else {
                        // Grab the disk number from the first extent element
                        disk_number = disk_extents.Extents[0].DiskNumber as i32;
                    }
                }

                let mut device_number = STORAGE_DEVICE_NUMBER::default();
                let res2 = unsafe {
                    DeviceIoControl(
                        opened_device.0,
                        IOCTL_STORAGE_GET_DEVICE_NUMBER,
                        None,
                        0,
                        Some(&mut device_number as *mut STORAGE_DEVICE_NUMBER as *mut _),
                        std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                        Some(&mut size),
                        None,
                    )
                };

                if res2.is_ok() {
                    disk_number = device_number.DeviceNumber as i32;
                }

                // If both failed or it's a RAID, you can skip this device loop iteration
                if disk_number == -1 {
                    continue;
                }

                let path_string = format!(r"\\.\PhysicalDrive{}", disk_number);

                (std::path::PathBuf::from(path_string), disk_number)
            };

            // Calculate the display path (Drive letters)
            let mountpoints = get_logical_mountpoints(disk_number as u32);
            let display_path = if mountpoints.is_empty() {
                // what is better things empty is is more usefull for the user
                // String::from("Unmounted")
                String::from("")
            } else {
                mountpoints.join(", ") // Formats multiple partitions as "D:\, E:\"
            };

            output.push(BlockDevice::new(
                display_path,
                path,
                name,
                size_bytes,
                is_removable,
                sector_size,
            ));
        }
        let _ = unsafe { SetupDiDestroyDeviceInfoList(devices) };
        Ok(output)
    }
}
/// Iterates logical drives (A-Z) and returns the drive letters mapped to the given physical disk.
fn get_logical_mountpoints(target_disk_number: u32) -> Vec<String> {
    let mut mountpoints = Vec::new();

    // Get a bitmask of all available logical drives (A=1, B=2, C=4)
    let logical_drives_mask = unsafe { GetLogicalDrives() };
    if logical_drives_mask == 0 {
        return mountpoints;
    }

    for i in 0..26 {
        if (logical_drives_mask & (1 << i)) != 0 {
            let letter = (b'A' + i) as char;
            let root_path = format!("{}:\\", letter);
            let root_wide: Vec<u16> = root_path.encode_utf16().chain(std::iter::once(0)).collect();

            // Only check fixed or removable drives (ignore CD-ROMs, RAM disks, network drives)
            let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_wide.as_ptr())) };
            if drive_type != windows::Win32::System::WindowsProgramming::DRIVE_FIXED
                && drive_type != windows::Win32::System::WindowsProgramming::DRIVE_REMOVABLE
            {
                continue;
            }

            // Open a handle to the logical volume (e.g., "\\.\C:")
            let device_path = format!(r"\\.\{}:", letter);
            let device_wide: Vec<u16> = device_path
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

            let handle = unsafe {
                CreateFileW(
                    PCWSTR(device_wide.as_ptr()),
                    0, // 0 is enough to query metadata; avoids needing Admin write privileges
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            };

            if let Ok(h) = handle {
                if !h.is_invalid() {
                    let auto_handle = AutoCloseHandle(h);
                    // Use your existing helper to extract the physical disk number
                    if let Some(disk_num) = storage_device_number(auto_handle.0) {
                        if disk_num == target_disk_number {
                            mountpoints.push(root_path);
                        }
                    }
                }
            }
        }
    }

    mountpoints
}
