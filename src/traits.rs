use std::io::SeekFrom;

use tokio_stream::Stream;

use crate::{
    data_types::{
        AsyncImageSourceFile,
        BlockDevice,
        DeviceEvent,
        FlashProgress,
    },
    error::FlashResult,
};
/// for os platform the flasher functions
pub trait FlasherGeneric<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector,
{
    /// verifyer
    async fn verify(
        &self,

        handle: &mut T::Handle,
        source: AsyncImageSourceFile,
        written_bytes: u64,
        send_progress: tokio::sync::watch::Sender<FlashProgress>,
    ) -> FlashResult<tokio::sync::watch::Sender<FlashProgress>>;

    /// flasher
    async fn flash(
        &self,
        source_of_image: AsyncImageSourceFile,
        device: &BlockDevice,
        on_progress: tokio::sync::watch::Sender<FlashProgress>,
    ) -> FlashResult<()>;
}
/// A raw write handle to a block device.
/// Separated from DeviceWriter so the handle can carry platform state
/// (Windows keeps the lock handle alive here).
pub trait RawWriteHandle {
    /// write to fill with offset
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()>;
    /// read to fill with offset
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<usize>;

    /// Flush kernel buffers → physical media. Must be called before Done!
    fn flush_to_disk(&mut self) -> FlashResult<()>;

    /// Sector size for this device.
    fn sector_size(&self) -> usize;

    /// Total writable size in bytes.
    fn size_bytes(&self) -> FlashResult<u64>;
    /// set the file to seek
    fn seek(&mut self, seek: SeekFrom) -> FlashResult<()>;
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
///   Linux   → dbus udisk2
///   macOS   → IOKit IOMedia registry  
///   Windows → win32
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
    /// Gives at startup the devices back and then watching
    /// Returns a channel receiver, caller drops it to stop watching.
    async fn watch_devices(&self) -> FlashResult<Self::WatchStream>;
}
/// Unmount all filesystems on a device before writing.
///   Linux   → via dbus and udisk2
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
#[derive(Debug)]
pub struct Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector + AsyncDeviceEnumerator,
{
    /// interface
    pub interface: T,
}

impl<T> Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector + AsyncDeviceEnumerator,
{
    /// basic constructor
    pub fn new(interface: T) -> Self {
        Self { interface }
    }
    /// Get all storage decies with intoformation
    pub async fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
        self.interface.list_devices().await
    }
    /// get async the storage devices
    pub async fn watch_devices(&self) -> FlashResult<<T as AsyncDeviceEnumerator>::WatchStream> {
        self.interface.watch_devices().await
    }
}
