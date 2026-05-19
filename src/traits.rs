use std::io;

use tokio::io::{
    AsyncBufReadExt,
    AsyncReadExt,
    AsyncSeekExt,
    AsyncWriteExt,
};
use tokio_stream::Stream;
use tracing::info;

use crate::{
    data_types::{
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
/// Decompress / stream an image file into a byte source.
/// Lets the write loop not care about image format.
pub trait ImageSource {
    /// Uncompressed size, if known ahead of time.
    fn uncompressed_size(&self) -> u64;

    /// Read the next chunk of uncompressed data.
    /// Returns 0 on EOF.
    fn read_chunk(&mut self, buf: &mut [u8]) -> FlashResult<usize>;

    /// SHA256 of the *uncompressed* content, if embedded in the image format.
    fn expected_hash(&self) -> Option<[u8; 32]> {
        None
    }
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
        source: impl ImageSource + tokio::io::AsyncRead + tokio::io::AsyncBufRead + Unpin,
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
        let mut reader = tokio::io::BufReader::with_capacity(self.chunk_size, handle);
        reader.fill_buf().await?;
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        let mut tmp_counter: u64 = 0;
        while !reader.buffer().is_empty() {
            hasher.update(reader.buffer());
            offset += u64::try_from(reader.buffer().len())?;
            tmp_counter += reader.buffer().len() as u64;
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
            reader.consume(reader.buffer().len());
            reader.fill_buf().await?;
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
        source: impl ImageSource + tokio::io::AsyncRead + tokio::io::AsyncBufRead + Unpin,
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

        let handle = self.interface.open_for_writing(device).await?;
        let sector_size = handle.sector_size();
        let total_bytes = source.uncompressed_size();
        #[cfg(target_os = "linux")]
        let (mut reader, mut writter) = (
            tokio::io::BufReader::with_capacity(self.chunk_size, source),
            tokio::io::BufWriter::with_capacity(self.chunk_size, handle),
        );

        #[cfg(not(target_os = "linux"))]
        let (mut reader, mut writter) = (
            tokio::io::BufReader::with_capacity(sector_size, source),
            tokio::io::BufWriter::with_capacity(sector_size, handle),
        );
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
        let mut status_read_back: usize = usize::MAX;
        let mut buffer = vec![0u8; sector_size];
        while status_read_back != 0 {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(number_read) => {
                    let buffer_write = match buffer.get(..number_read) {
                        Some(x) => x,
                        None => return Err(FlashError::OutOfBoundsArray),
                    };
                    status_read_back = number_read;
                    writter.write_all(buffer_write).await?;
                    offset += number_read as u64;
                    bytes_since_last_report += number_read as u64;
                }
                Err(error) => {
                    tracing::error!("Read error: {}", error);
                    return Err(FlashError::Io(error));
                }
            }

            let elapsed = timer.elapsed().as_secs_f64();
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
        on_progress
            .send(FlashProgress::transition(FlashPhase::Flushing))
            .map_err(|_| FlashError::SendChannelError)?;
        writter.flush().await?;
        let mut handle = writter.into_inner();
        handle.flush_to_disk().await?;
        handle.seek(io::SeekFrom::Start(0)).await?;
        info!("Verify now");
        on_progress
            .send(FlashProgress::transition(FlashPhase::Verifying))
            .map_err(|_| FlashError::SendChannelError)?;

        let source = reader.into_inner();
        let sender = self
            .verify(&mut handle, source, offset, on_progress)
            .await?;

        self.interface.eject(device).await?;

        sender
            .send(FlashProgress::transition(FlashPhase::Done))
            .map_err(|_| FlashError::SendChannelError)?;
        Ok(())
    }
}
