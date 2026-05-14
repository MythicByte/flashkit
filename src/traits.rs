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
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()>;
    /// read to fill with offset
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()>;

    /// Flush kernel buffers → physical media. Must be called before Done.
    fn flush_to_disk(&mut self) -> FlashResult<()>;

    /// Sector size for this device. Writes must be multiples on Windows.
    fn sector_size(&self) -> u32;

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
    fn open_for_writing(&self, device: &BlockDevice) -> FlashResult<Self::Handle>;
}

/// Enumerate block devices on the system.
/// Each platform reads from a different source:
///   Linux   → /sys/block + udev
///   macOS   → IOKit IOMedia registry  
///   Windows → SetupDi / WMI
pub trait DeviceEnumerator {
    /// list all storage devices
    fn list_devices(&self) -> FlashResult<Vec<BlockDevice>>;

    /// Watch for hotplug events (USB insert/remove).
    /// Returns a channel receiver; caller drops it to stop watching.
    fn watch_devices(&self) -> FlashResult<std::sync::mpsc::Receiver<DeviceEvent>> {
        // Default: unsupported — platforms can opt in
        Err(FlashError::UnsportedFeature)
    }
}
/// Unmount all filesystems on a device before writing.
///   Linux   → umount2() syscall via nix
///   macOS   → DADiskUnmount() via DiskArbitration
///   Windows → FSCTL_LOCK_VOLUME + FSCTL_DISMOUNT_VOLUME
pub trait DeviceUnmounter {
    /// Unmount all partitions.
    fn unmount_all(&self, device: &BlockDevice) -> FlashResult<()>;

    /// Check if any partition is still mounted
    ///
    /// Some(_) means it is mounted
    fn check_is_fully_unmounted(&self, device: &BlockDevice) -> FlashResult<bool>;
}

/// Eject the device after flashing so the user can safely remove it.
pub trait DeviceEjector {
    /// eject device
    fn eject(&self, device: &BlockDevice) -> FlashResult<()>;
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
#[derive(Debug)]
pub struct Flasher<E, U, W, J>
where
    E: DeviceEnumerator,
    U: DeviceUnmounter,
    W: DeviceWriter,
    J: DeviceEjector,
{
    enumerator: E,
    unmounter: U,
    writer: W,
    ejector: J,
    chunk_size: usize,
}

impl<E, U, W, J> Flasher<E, U, W, J>
where
    E: DeviceEnumerator,
    U: DeviceUnmounter,
    W: DeviceWriter,
    J: DeviceEjector,
{
    /// basic constructor
    pub fn new(enumerator: E, unmounter: U, writer: W, ejector: J, chunk_size: usize) -> Self {
        Self {
            enumerator,
            unmounter,
            writer,
            ejector,
            chunk_size,
        }
    }
    /// Get all storage decies with intoformation
    pub fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
        self.enumerator.list_devices()
    }
    /// flashes file to storage
    pub fn flash(
        &self,
        mut source: impl ImageSource,
        device: &BlockDevice,
        on_progress: impl Fn(FlashProgress),
    ) -> FlashResult<()> {
        // 1. Unmount
        on_progress(FlashProgress::phase(FlashPhase::Unmounting));
        self.unmounter.unmount_all(device)?;

        if self.unmounter.check_is_fully_unmounted(device)? {
            return Err(FlashError::DeviceBusy {
                path: device.path.clone(),
            });
        }

        // 2. Open raw handle
        let mut handle = self.writer.open_for_writing(device)?;
        let total_bytes = source.uncompressed_size();
        let mut buf = vec![0u8; self.chunk_size];
        let mut offset: u64 = 0;

        // 3. Write loop
        on_progress(FlashProgress {
            bytes_written: 0,
            total_bytes,
            bytes_per_sec: 0.0,
            phase: FlashPhase::Writing,
        });

        let mut timer = std::time::Instant::now();
        let mut bytes_since_last_report: u64 = 0;
        let mut n: usize = usize::MAX;
        while n != 0 {
            // Align chunk to sector boundary for Windows compatibility
            let aligned_len = align_down(buf.len(), handle.sector_size().try_into()?);
            let buffer_read = buf
                .get_mut(..aligned_len)
                .ok_or(FlashError::OutOfBoundsArray)?;
            n = source.read_chunk(buffer_read)?;

            // On Windows: pad last chunk to sector boundary
            let write_len = align_up(n, handle.sector_size().try_into()?);
            let buffer_write = buf.get(..write_len).ok_or(FlashError::OutOfBoundsArray)?;
            handle.write_at(offset, buffer_write)?;

            offset += u64::try_from(n)?; // track real bytes, not padded
            bytes_since_last_report += u64::try_from(n)?;

            let elapsed = timer.elapsed().as_secs_f64();
            if elapsed >= 0.25 {
                on_progress(FlashProgress {
                    bytes_written: offset,
                    total_bytes,
                    bytes_per_sec: bytes_since_last_report as f64 / elapsed,
                    phase: FlashPhase::Writing,
                });
                bytes_since_last_report = 0;
                timer = std::time::Instant::now();
            }
        }

        // 4. Flush
        on_progress(FlashProgress::phase(FlashPhase::Flushing));
        handle.flush_to_disk()?;

        // 5. Verify
        on_progress(FlashProgress::phase(FlashPhase::Verifying));
        self.verify(&mut handle, &mut source, offset)?;

        // 6. Eject
        self.ejector.eject(device)?;

        on_progress(FlashProgress::phase(FlashPhase::Done));
        Ok(())
    }

    fn verify(
        &self,
        handle: &mut W::Handle,
        source: &mut impl ImageSource,
        written_bytes: u64,
    ) -> FlashResult<()> {
        use sha2::{
            Digest,
            Sha256,
        };
        let mut buf = vec![0u8; self.chunk_size];
        let mut hasher = Sha256::new();
        let mut offset = 0u64;

        while offset < written_bytes {
            let to_read = buf
                .len()
                .min((written_bytes.saturating_sub(offset)).try_into()?);
            let buffer = buf.get_mut(..to_read).ok_or(FlashError::OutOfBoundsArray)?;
            handle.read_at(offset, buffer)?;
            hasher.update(&buffer);
            offset += u64::try_from(to_read)?;
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

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Alignment helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Round `n` down to the nearest multiple of `align`.
///
/// `align` **must** be a power of two; this is asserted in debug builds.
#[inline]
pub(crate) fn align_down(n: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    n & !(align - 1)
}

/// Round `n` up to the nearest multiple of `align`.
///
/// `align` **must** be a power of two; this is asserted in debug builds.
#[inline]
pub(crate) fn align_up(n: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two(), "align must be a power of two");
    (n + align - 1) & !(align - 1)
}
