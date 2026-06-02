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
use serde::Deserialize;
use std::{
    collections::HashSet,
    io::{
        Seek,
        SeekFrom,
    },
    iter::once,
    mem::size_of,
    os::windows::{
        fs::FileExt,
        io::FromRawHandle,
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
        Foundation::{
            CloseHandle,
            HANDLE,
        },
        Storage::FileSystem::{
            CreateFileW,
            DeleteVolumeMountPointW,
            FILE_FLAGS_AND_ATTRIBUTES,
            FILE_GENERIC_READ,
            FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_SHARE_WRITE,
            FindFirstVolumeW,
            FindNextVolumeW,
            FindVolumeClose,
            GetVolumePathNamesForVolumeNameW,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            OPEN_EXISTING,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                FSCTL_DISMOUNT_VOLUME,
                FSCTL_LOCK_VOLUME,
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
/// The raw HANDLE is kept private; all access goes through the typed methods
/// below.  This prevents the Copy-able raw value from "escaping" and being
/// used independently of its owner.
#[derive(Debug)]
struct VolumeFindHandle(HANDLE);

impl VolumeFindHandle {
    /// Begin volume enumeration.  Writes the first volume's GUID path into
    /// `buf` and returns the guard that owns the cursor.
    ///
    /// A volume GUID path looks like `\\?\Volume{xxxxxxxx-...}\` — a stable,
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

#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct WindowsInterface;

#[allow(missing_docs)]
#[derive(Debug)]
pub struct WindowsRawWriteHandle {
    file: std::fs::File,
    sector_size: usize,
    size_bytes: u64,
    /// Keeps the FSCTL_LOCK_VOLUME handles alive for every volume on this
    /// physical drive.  Dropping them releases the locks and lets Windows
    /// remount the filesystems, so they must outlive every write/flush.
    _volume_locks: Vec<AutoCloseHandle>,
}

impl RawWriteHandle for WindowsRawWriteHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        self.file
            .seek_write(buf, offset)
            .map_err(|e| FlashError::Io(e))?;

        Ok(())
    }

    /// Positional read via `ReadFile` + `OVERLAPPED` — mirror of `write_at`.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize> {
        let bytes_read = self
            .file
            .seek_read(buf, offset)
            .map_err(|e| FlashError::Io(e))?;
        Ok(bytes_read)
    }

    /// Flush kernel write buffers to physical media via `FlushFileBuffers`.
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
        self.file.seek(seek).map_err(|e| FlashError::Io(e))?;
        Ok(())
    }
}

impl DeviceWriter for WindowsInterface {
    type Handle = WindowsRawWriteHandle;

    /// Acquire and hold volume locks BEFORE opening the write handle.
    /// unmount_volumes_on_drive_locked returns the lock handles; as long as
    /// they stay alive inside WindowsRawWriteHandle, Windows cannot remount
    /// the filesystem and interrupt our writes mid-flash.
    async fn open_for_writing(&self, device: &BlockDevice) -> FlashResult<Self::Handle> {
        let path = device.path.clone();
        let sector_size = device.sector_size;
        let size_bytes = device.size_bytes;

        let volume_locks = unmount_volumes_on_drive_locked(&path)?;

        let path_str = path.to_string_lossy().to_string();
        let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();

        // open the device
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
        if handle.is_invalid() {
            return Err(FlashError::WindowsHandle);
        }

        let file = unsafe { std::fs::File::from_raw_handle(handle.0 as _) };
        Ok(WindowsRawWriteHandle {
            file,
            sector_size,
            size_bytes,
            _volume_locks: volume_locks,
        })
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

            if handle.is_invalid() {
                return Err(FlashError::SyncError);
            }
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
        tokio::task::spawn_blocking(move || unmount_volumes_on_drive_locked(&path).map(|_| ()))
            .await
            .map_err(|_| FlashError::SyncError)?
    }
}

/// Dismount every volume on `physical_path` and **return the lock handles**.
///
/// The caller must keep the returned `Vec<AutoCloseHandle>` alive for the
/// entire flash operation.  Dropping them releases `FSCTL_LOCK_VOLUME` and
/// lets Windows remount the filesystems, corrupting mid-flash writes.
fn unmount_volumes_on_drive_locked(physical_path: &Path) -> FlashResult<Vec<AutoCloseHandle>> {
    const VOLUME_GUID_BUF_LEN: usize = 64;
    let mut vol_buf = vec![0u16; VOLUME_GUID_BUF_LEN];

    let target_number = physical_drive_number(physical_path)?;

    // The guard calls `FindVolumeClose` when it drops.
    let finder = VolumeFindHandle::start(&mut vol_buf)?;
    let mut locks: Vec<AutoCloseHandle> = Vec::new();

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

// `GetVolumePathNamesForVolumeNameW` returns all of a volume's mount points
// as a "multi-string": consecutive null-terminated UTF-16 strings packed into
// one buffer, ending with an extra null:
//
//   C:\<NUL>D:\Data\<NUL><NUL>
//
// The safe approach is the two-pass pattern:
//   Pass 1 — call with a 1-element probe buffer.  The call fails but Windows
//             sets `required_chars` to the number of UTF-16 code units needed.
//   Pass 2 — allocate exactly that many code units and call again.
//
// A fixed-size buffer (as the original used) silently fails when a drive has
// many or long mount points, leaving them attached and causing the later
// FSCTL_DISMOUNT_VOLUME to produce incomplete results.

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
        return; // no mount points, or volume not accessible
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

///
fn process_single_volume(
    guid_path: &str,
    target_number: u32,
) -> FlashResult<Option<AutoCloseHandle>> {
    let device_path = guid_path.trim_end_matches('\\');
    let device_wide: Vec<u16> = device_path.encode_utf16().chain(once(0)).collect();

    // Query handle
    let query_handle = unsafe {
        CreateFileW(
            PCWSTR(device_wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| FlashError::FilesystemError(format!("Failed to query volume attributes: {e}")))?;

    let query_guard = AutoCloseHandle(query_handle);
    let is_match = storage_device_number(query_handle) == Some(target_number);
    drop(query_guard);

    if !is_match {
        return Ok(None);
    }

    // Remove drive letters / mount points
    remove_mount_points(guid_path);

    // Open a write handle.
    let write_handle = unsafe {
        CreateFileW(
            PCWSTR(device_wide.as_ptr()),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| FlashError::FilesystemError(format!("Access denied opening volume: {e}")))?;

    let lock_guard = AutoCloseHandle(write_handle);
    let mut bytes_returned = 0u32;

    let mut locked = false;
    for _ in 0..10 {
        if unsafe {
            DeviceIoControl(
                write_handle,
                FSCTL_LOCK_VOLUME,
                None,
                0,
                None,
                0,
                Some(&mut bytes_returned),
                None,
            )
            .is_ok()
        } {
            locked = true;
            break;
        }
        thread::sleep(Duration::from_millis(250)); // Wait for AV to release the drive
    }

    if !locked {
        return Err(FlashError::FilesystemError(
            "Failed to lock volume (files may be in use by Anti-Virus)".into(),
        ));
    }

    // Dismount the filesystem
    unsafe {
        DeviceIoControl(
            write_handle,
            FSCTL_DISMOUNT_VOLUME,
            None,
            0,
            None,
            0,
            Some(&mut bytes_returned),
            None,
        )
    }
    .map_err(|e| FlashError::FilesystemError(format!("Failed to dismount volume: {e}")))?;

    Ok(Some(lock_guard))
}
/// Query the system for all mount points (drive letters or paths)
/// associated with a specific physical drive number.
/// for getting the letter name
pub fn get_drive_letters_for_drive(device: BlockDevice) -> Vec<String> {
    let mut letters = Vec::new();

    // 1. Safely extract the drive number (e.g., 2 from "\\.\PhysicalDrive2")
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
            FILE_SHARE_READ | FILE_SHARE_WRITE,
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
            break; // Double-null terminator reached
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
impl AsyncDeviceEnumerator for WindowsInterface {
    type WatchStream = ReceiverStream<DeviceEvent>;

    async fn watch_devices(&self) -> FlashResult<Self::WatchStream> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        //  Fetch current devices and send them immediately
        let initial_devices = self.list_devices().await?;

        for dev in initial_devices.clone() {
            if tx.send(DeviceEvent::Added(dev)).await.is_err() {
                return Ok(ReceiverStream::new(rx));
            }
        }

        let enumerator = self.clone();

        tokio::spawn(async move {
            let (wake_tx, mut wake_rx) = tokio::sync::mpsc::unbounded_channel();

            tokio::task::spawn_blocking(move || {
                let wmi_con = match WMIConnection::new() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("WMI connection failed: {e}");
                        return;
                    }
                };

                // Query listens for creations, deletions, and modifications of Disk Drives
                let query = "SELECT * FROM __InstanceOperationEvent WITHIN 2 WHERE TargetInstance ISA 'Win32_DiskDrive'";

                let iterator = match wmi_con.exec_notification_query(query) {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::error!("WMI query failed: {e}");
                        return;
                    }
                };

                // Iterate over notifications natively provided by the crate
                for _event in iterator {
                    if wake_tx.send(()).is_err() {
                        break; // The receiver was dropped, shut down the thread
                    }
                }
            });

            let mut known_paths: HashSet<PathBuf> =
                initial_devices.into_iter().map(|d| d.path).collect();

            while wake_rx.recv().await.is_some() {
                // A slight delay ensures Windows Volume Manager finishes mounting operations
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;

                // Re-scan active drives
                let current_devices = match enumerator.list_devices().await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("Failed to re-enumerate devices during WMI wake: {e}");
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
                        return;
                    }
                }
            }
        });

        Ok(ReceiverStream::new(rx))
    }
}
