use std::{
    collections::HashSet,
    io::{
        Seek,
        SeekFrom,
    },
    mem::MaybeUninit,
    os::fd::{
        AsFd,
        BorrowedFd,
        OwnedFd,
    },
    path::PathBuf,
};

use rustix::{
    cmsg_space,
    fs::{
        OFlags,
        fcntl_nocache,
    },
    io::{
        FdFlags,
        fcntl_setfd,
    },
    net::{
        RecvAncillaryBuffer,
        RecvAncillaryMessage,
        RecvFlags,
    },
};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    data_types::DeviceEvent,
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
#[derive(Debug, Clone)]
pub struct DarwinInterface;

#[allow(missing_docs)]
#[derive(Debug)]
pub struct DarwinRawWriteHandle {
    file: std::fs::File,
    sector_size: usize,
    size_bytes: u64,
}
impl RawWriteHandle for DarwinRawWriteHandle {
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
impl DeviceWriter for DarwinInterface {
    type Handle = DarwinRawWriteHandle;

    /// get a file descriptor with authopen
    async fn open_for_writing(
        &self,
        device: &crate::data_types::BlockDevice,
    ) -> crate::error::FlashResult<Self::Handle> {
        let raw_path = to_raw_device_path(&device.path)
            .to_string_lossy()
            .to_string();
        let sector_size = device.sector_size;
        let size_bytes = device.size_bytes;
        let file = tokio::task::spawn_blocking(move || -> FlashResult<std::fs::File> {
            // O_RDWR  — read/write access.
            // O_SYNC  — every write is flushed to the device before returning.
            //
            // macOS has no O_DIRECT.  Kernel buffer-cache bypass is applied via
            // fcntl(F_NOCACHE) after we receive the fd, it cannot.
            //
            // O_CLOEXEC is NOT forwarded to authopen — authopen opens the device
            // on our behalf and the flags control *its* open() call.  We set
            // FD_CLOEXEC on the received fd ourselves with fcntl_setfd below.
            let open_flags = (OFlags::RDWR | OFlags::SYNC).bits();

            let (parent_sock, child_sock) = rustix::net::socketpair(
                rustix::net::AddressFamily::UNIX,
                rustix::net::SocketType::STREAM,
                rustix::net::SocketFlags::empty(),
                None,
            )
            .map_err(|e| {
                FlashError::FilesystemError(format!("failed to create socketpair: {e}"))
            })?;

            // Spawn authopen and capture stdout so we can call recvmsg(2) on it.
            let mut child = std::process::Command::new("authopen")
                .args([
                    "-stdoutpipe", // deliver the fd via SCM_RIGHTS on stdout
                    "-o",
                    &open_flags.to_string(),
                    &raw_path,
                ])
                .stdout(std::process::Stdio::from(child_sock))
                .spawn()
                .map_err(|e| {
                    FlashError::FilesystemError(format!("failed to spawn authopen: {e}"))
                })?;

            // recvmsg blocks until authopen either sends the fd or closes the pipe.
            // OwnedFd returned here closes automatically on any subsequent error.
            let owned_fd: OwnedFd = recv_fd_from_authopen(parent_sock.as_fd())?;

            // Reap the child to avoid zombies.  If authopen signals failure even
            // though we received a valid fd, owned_fd is dropped (closed) here.
            let status = child.wait().map_err(FlashError::Io)?;
            if !status.success() {
                // owned_fd dropped and closed automatically — no libc::close needed.
                return Err(FlashError::FilesystemError(format!(
                    "authopen exited with {status} while opening {raw_path}"
                )));
            }

            //  FD_CLOEXEC: prevent the device fd leaking into child processes.
            fcntl_setfd(&owned_fd, FdFlags::CLOEXEC)
                .map_err(|e| FlashError::Io(std::io::Error::from(e)))?;

            //  F_NOCACHE: bypass the kernel unified buffer cache for this fd.
            //    This is the macOS equivalent of Linux's O_DIRECT.  It must be
            //    set post-open via fcntl — there is no open(2) flag equivalent.
            fcntl_nocache(&owned_fd, true).map_err(|e| FlashError::Io(std::io::Error::from(e)))?;

            // Safe conversion: OwnedFd is a fully configured, exclusively owned fd.
            Ok(std::fs::File::from(owned_fd))
        })
        .await
        .map_err(|_| FlashError::SyncError)??;

        Ok(DarwinRawWriteHandle {
            file,
            sector_size,
            size_bytes,
        })
    }
}
impl DeviceEnumerator for DarwinInterface {
    async fn list_devices(&self) -> crate::error::FlashResult<Vec<crate::data_types::BlockDevice>> {
        let output = tokio::process::Command::new("diskutil")
            .args(["list", "-plist"])
            .output()
            .await
            .map_err(FlashError::Io)?;

        if !output.status.success() {
            return Err(FlashError::FilesystemError(
                "Failed executing diskutil list".into(),
            ));
        }

        let dict: plist::Value = plist::from_bytes(&output.stdout)
            .map_err(|e| FlashError::FilesystemError(e.to_string()))?;

        let mut devices = Vec::new();

        if let Some(whole_disks) = dict
            .as_dictionary()
            .and_then(|d| d.get("WholeDisks"))
            .and_then(|w| w.as_array())
        {
            for disk_val in whole_disks {
                if let Some(disk_str) = disk_val.as_string() {
                    // Ignore disk images/synthetics; query real attributes for individual whole disks
                    if let Ok(device) = fetch_disk_info(disk_str).await {
                        devices.push(device);
                    }
                }
            }
        }

        Ok(devices)
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
/// Convert a buffered device path to its raw counterpart.
///
/// `/dev/disk2`  →  `/dev/rdisk2`
/// `/dev/rdisk2` →  `/dev/rdisk2`  (already raw; no-op)
///
/// Anything that does not match the expected `/dev/diskN` pattern is returned
/// unchanged so callers can still attempt the open and surface the OS error.
fn to_raw_device_path(path: &std::path::Path) -> PathBuf {
    match path.file_name().and_then(|n| n.to_str()) {
        // Already a raw node.
        Some(name) if name.starts_with("rdisk") => path.to_path_buf(),
        // Buffered node – prepend 'r'.
        Some(name) if name.starts_with("disk") => PathBuf::from(format!("/dev/r{}", name)),
        // Unknown format – pass through and let the OS complain.
        _ => path.to_path_buf(),
    }
}
/// Helper function to parse individual disk attributes natively
async fn fetch_disk_info(disk_identifier: &str) -> FlashResult<crate::data_types::BlockDevice> {
    let output = tokio::process::Command::new("diskutil")
        .args(["info", "-plist", disk_identifier])
        .output()
        .await
        .map_err(FlashError::Io)?;

    let dict: plist::Value = plist::from_bytes(&output.stdout)
        .map_err(|e| FlashError::FilesystemError(e.to_string()))?;

    let d = dict
        .as_dictionary()
        .ok_or_else(|| FlashError::FilesystemError("Invalid plist structure".into()))?;

    let path_str = d
        .get("DeviceNode")
        .and_then(|v| v.as_string())
        .unwrap_or("");
    let path = std::path::PathBuf::from(path_str);

    let size_bytes = d
        .get("TotalSize")
        .and_then(|v| v.as_unsigned_integer())
        .unwrap_or(0);
    let sector_size = d
        .get("DeviceBlockSize")
        .and_then(|v| v.as_unsigned_integer())
        .unwrap_or(512) as usize;
    let is_removable = d
        .get("RemovableMedia")
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);

    let name = d
        .get("MediaName")
        .or_else(|| d.get("DeviceIdentifier"))
        .and_then(|v| v.as_string())
        .unwrap_or("Unknown Drive")
        .to_string();

    Ok(crate::data_types::BlockDevice {
        display_path: path.to_str().unwrap_or_default().to_string(),
        path,
        name,
        size_bytes,
        is_removable,
        sector_size,
    })
}
/// Receive the file descriptor that `authopen -stdoutpipe` sends via SCM_RIGHTS.
///
/// authopen delivers the opened device fd as a single `SCM_RIGHTS` ancillary
/// control message with one null byte as the regular data payload.  A plain
/// `read()` will never surface the fd — `recvmsg(2)` is required.
///
/// The returned [`OwnedFd`] closes automatically on drop, so every early-return
/// error path is leak-free with no manual `close` calls.
fn recv_fd_from_authopen(pipe: BorrowedFd<'_>) -> FlashResult<OwnedFd> {
    // Allocate a control-message buffer sized for exactly one SCM_RIGHTS fd.
    // cmsg_space! accounts for the cmsghdr header + alignment padding.
    let mut cmsg_buf = vec![MaybeUninit::<u8>::uninit(); cmsg_space!(ScmRights(1))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut cmsg_buf);

    // authopen writes one null byte as the regular data portion of the message.
    let mut data_byte = [0u8; 1];
    let mut iov = [std::io::IoSliceMut::new(&mut data_byte)];

    let result = rustix::net::recvmsg(pipe, &mut iov, &mut ancillary, RecvFlags::empty())
        .map_err(|e| FlashError::Io(std::io::Error::from(e)))?;

    // EOF on the pipe means the user cancelled the auth dialog or authopen
    // could not open the device — no fd will ever arrive.
    if result.bytes == 0 {
        return Err(FlashError::FilesystemError(
            "authopen: authorisation denied or device unavailable".into(),
        ));
    }

    // Walk the control message chain and extract the first SCM_RIGHTS fd.
    // Any extra unexpected fds yielded by the iterator are dropped (and thus
    // closed) here automatically via OwnedFd's Drop impl.
    for msg in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(mut fds) = msg {
            if let Some(fd) = fds.next() {
                return Ok(fd);
            }
        }
    }

    Err(FlashError::FilesystemError(
        "authopen: control message absent — no file descriptor received".into(),
    ))
}
impl AsyncDeviceEnumerator for DarwinInterface {
    type WatchStream = ReceiverStream<DeviceEvent>;

    async fn watch_devices(&self) -> FlashResult<Self::WatchStream> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        //  Establish the identical uniform baseline snapshot instantly
        let initial_devices = self.list_devices().await?;
        let mut known_paths: HashSet<PathBuf> = HashSet::new();

        for dev in initial_devices {
            known_paths.insert(dev.path.clone());
            if tx.send(DeviceEvent::Added(dev)).await.is_err() {
                return Ok(ReceiverStream::new(rx));
            }
        }

        let enumerator = self.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            // Missed ticks are skipped because we explicitly poll on a fixed clock
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                // Query the updated hardware state via your existing diskutil parser
                let current_devices = match enumerator.list_devices().await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("macOS device watcher failed to list devices: {e}");
                        continue;
                    }
                };

                let current_paths: HashSet<PathBuf> =
                    current_devices.iter().map(|d| d.path.clone()).collect();

                // Check for insertions
                for dev in current_devices {
                    if known_paths.insert(dev.path.clone()) {
                        if tx.send(DeviceEvent::Added(dev)).await.is_err() {
                            return; // Consumer dropped the stream receiver, exit task safely
                        }
                    }
                }

                // Check for removals
                let dead_paths: Vec<PathBuf> =
                    known_paths.difference(&current_paths).cloned().collect();

                for path in dead_paths {
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
