use std::io::{
    self,
};

use tokio::io::{
    AsyncReadExt,
    AsyncSeekExt,
    AsyncWriteExt,
};
use tokio_stream::Stream;
use tracing::info;

use crate::{
    aligned::PageAlignedBuffer,
    data_types::{
        AsyncImageSourceFile,
        BlockDevice,
        DeviceEvent,
        FlashPhase,
        FlashProgress,
    },
    error::{
        FlashError,
        FlashResult,
    },
};

/// A raw write handle to a block device.
/// Separated from DeviceWriter so the handle can carry platform state
/// (e.g. Windows keeps the lock handle alive here).
pub trait RawWriteHandle {
    /// write to fill with offset
    async fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()>;
    /// read to fill with offset
    async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()>;

    /// Flush kernel buffers → physical media. Must be called before Done.
    async fn flush_to_disk(&mut self) -> FlashResult<()>;

    /// Sector size for this device. Writes must be multiples on Windows.
    fn sector_size(&self) -> usize;

    /// Total writable size in bytes.
    fn size_bytes(&self) -> FlashResult<u64>;
}
/// Open a raw writable handle to a block device.
/// Abstracted because Windows requires different open flags,
/// sector-aligned writes, and the handle must stay open post-lock.
pub trait DeviceWriter {
    /// wrte handle
    type Handle: RawWriteHandle;

    /// open file with lock
    async fn open_for_writing(&self, device: &BlockDevice) -> FlashResult<Self::Handle>;
}

/// Enumerate block devices on the system.
/// Each platform reads from a different source:
///   Linux   → /sys/block + udev
///   macOS   → IOKit IOMedia registry  
///   Windows → SetupDi / WMI
pub trait DeviceEnumerator {
    /// list all storage devices
    async fn list_devices(&self) -> FlashResult<Vec<BlockDevice>>;
}
/// listening devices async
pub trait AsyncDeviceEnumerator: DeviceEnumerator {
    /// the stream to give back of the events
    type WatchStream: Stream<Item = DeviceEvent> + Send + Unpin + 'static;
    /// watches a device async
    ///
    /// Watch for hotplug events (USB insert/remove).
    /// Returns a channel receiver; caller drops it to stop watching.
    fn watch_devices(
        &self,
    ) -> impl std::future::Future<Output = FlashResult<Self::WatchStream>> + Send + '_;
    /// Gives at startup the devices back and then watching
    ///
    /// Watch for hotplug events (USB insert/remove).
    /// Returns a channel receiver; caller drops it to stop watching.
    fn watch_devices_with_initial(
        &self,
    ) -> impl std::future::Future<Output = FlashResult<Self::WatchStream>> + Send + '_;
}
/// Unmount all filesystems on a device before writing.
///   Linux   → umount2() syscall via nix
///   macOS   → DADiskUnmount() via DiskArbitration
///   Windows → FSCTL_LOCK_VOLUME + FSCTL_DISMOUNT_VOLUME
pub trait DeviceUnmounter {
    /// Unmount all partitions.
    async fn unmount(&self, device: &BlockDevice) -> FlashResult<()>;
}

/// Eject the device after flashing so the user can safely remove it.
pub trait DeviceEjector {
    /// eject device
    async fn eject(&self, device: &BlockDevice) -> FlashResult<()>;
}

/// Generic Flasher
#[derive(Debug, Clone)]
pub struct Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector,
{
    interface: T,
    chunk_size: usize,
}

impl<T> Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector,
{
    /// basic constructor
    pub fn new(interface: T, chunk_size: usize) -> Self {
        Self {
            interface,
            chunk_size,
        }
    }
    /// Get all storage decies with intoformation
    pub async fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
        self.interface.list_devices().await
    }

    async fn verify(
        &self,
        handle: &mut T::Handle,
        source: AsyncImageSourceFile,
        written_bytes: u64,
        send_progress: tokio::sync::watch::Sender<FlashProgress>,
    ) -> FlashResult<tokio::sync::watch::Sender<FlashProgress>>
    where
        T::Handle: tokio::io::AsyncRead + Unpin,
    {
        use sha2::{
            Digest,
            Sha256,
        };
        let mut timer = std::time::Instant::now();
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        let mut tmp_counter: u64 = 0;
        let buffer = PageAlignedBuffer::new(1024).expect("error");
        let buffer_slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer.size()) };
        loop {
            if offset >= written_bytes {
                break;
            }
            let max_to_read = std::cmp::min(buffer.size() as u64, written_bytes - offset) as usize;
            let read_back = handle.read(&mut buffer_slice[..max_to_read]).await?;
            info!("Read: {}", read_back);
            if read_back == 0 {
                break;
            }
            hasher.update(&buffer_slice[..read_back]);
            offset += read_back as u64;
            tmp_counter += read_back as u64;
            let elapsed = timer.elapsed().as_secs_f64();
            if elapsed >= 1.00 {
                send_progress
                    .send(FlashProgress {
                        bytes_written: offset,
                        total_bytes: written_bytes,
                        bytes_per_sec: tmp_counter as f64 / elapsed,
                        phase: FlashPhase::Verifying,
                    })
                    .map_err(|_| FlashError::SendChannelError)?;
                tmp_counter = 0;
                timer = std::time::Instant::now();
            }
        }

        let actual = hasher.finalize();

        if let Some(expected) = source.expected_hash()
            && actual.as_slice() != expected
        {
            let failing_hash_hex = hex::encode(actual);
            let correct_hash_hex = hex::encode(expected);
            return Err(FlashError::VerificationFailed {
                failed_hash_hex: failing_hash_hex,
                expected_hex: correct_hash_hex,
            });
        }

        Ok(send_progress)
    }
    /// flash async versions
    pub async fn flash(
        &self,
        mut source_of_image: AsyncImageSourceFile,
        device: &BlockDevice,
        on_progress: tokio::sync::watch::Sender<FlashProgress>,
    ) -> FlashResult<()>
    where
        T::Handle: tokio::io::AsyncWrite + Unpin + tokio::io::AsyncRead + tokio::io::AsyncSeek,
    {
        on_progress
            .send(FlashProgress::transition(FlashPhase::Unmounting))
            .map_err(|_| FlashError::SendChannelError)?;
        self.interface.unmount(device).await?;

        let mut handle_target_write_to = self.interface.open_for_writing(device).await?;
        info!("{}", handle_target_write_to.sector_size());
        let total_bytes = source_of_image.uncompressed_size();
        let mut offset: u64 = 0;

        on_progress
            .send(FlashProgress {
                bytes_written: 0,
                total_bytes,
                bytes_per_sec: 0.0,
                phase: FlashPhase::Writing,
            })
            .map_err(|_| FlashError::SendChannelError)?;

        let mut timer = std::time::Instant::now();
        let mut bytes_since_last_report: u64 = 0;
        let buffer = PageAlignedBuffer::new(1024).expect("error");
        let mut buffer_slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer.size()) };
        loop {
            let read_back = source_of_image.file.read(&mut buffer_slice).await?;

            info!("Read: {}", read_back);
            if read_back < buffer.size() {
                if read_back == 0 {
                    break;
                }
                if let Some(slice) = buffer_slice.get_mut(read_back..) {
                    for i in slice {
                        *i = 0;
                    }
                }
                info!("Read: 1");
                handle_target_write_to.write_all(&buffer_slice).await?;
                offset += read_back as u64;
                break;
            } else {
                info!("Read: 2");
                handle_target_write_to.write_all(&buffer_slice).await?;
                info!("Read: after writing");
                offset += read_back as u64
            }

            let elapsed = timer.elapsed().as_secs_f64();
            bytes_since_last_report += read_back as u64;
            if elapsed >= 1.00 {
                on_progress
                    .send(FlashProgress {
                        bytes_written: offset,
                        total_bytes,
                        bytes_per_sec: bytes_since_last_report as f64 / elapsed,
                        phase: FlashPhase::Writing,
                    })
                    .map_err(|_| FlashError::SendChannelError)?;
                bytes_since_last_report = 0;
                timer = std::time::Instant::now();
            }
        }
        info!("Flash");
        on_progress
            .send(FlashProgress::transition(FlashPhase::Flushing))
            .map_err(|_| FlashError::SendChannelError)?;
        handle_target_write_to.flush_to_disk().await?;
        handle_target_write_to.seek(io::SeekFrom::Start(0)).await?;
        info!("Verify now");
        on_progress
            .send(FlashProgress::transition(FlashPhase::Verifying))
            .map_err(|_| FlashError::SendChannelError)?;

        let sender = self
            .verify(
                &mut handle_target_write_to,
                source_of_image,
                offset,
                on_progress,
            )
            .await?;

        self.interface.eject(device).await?;

        sender
            .send(FlashProgress::transition(FlashPhase::Done))
            .map_err(|_| FlashError::SendChannelError)?;
        Ok(())
    }
}
