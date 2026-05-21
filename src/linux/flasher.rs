use tokio::io::AsyncReadExt;
use tracing::info;

use crate::{
    aligned::PageAlignedBuffer,
    data_types::{
        AsyncImageSourceFile,
        BlockDevice,
        FlashPhase,
        FlashProgress,
    },
    error::{
        FlashError,
        FlashResult,
    },
    traits::{
        DeviceEjector,
        DeviceEnumerator,
        DeviceUnmounter,
        DeviceWriter,
        Flasher,
        FlasherGeneric,
        RawWriteHandle,
    },
};

impl<T> FlasherGeneric<T> for Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector,
{
    /// flash async versions
    async fn flash(
        &self,
        mut source_of_image: AsyncImageSourceFile,
        device: &BlockDevice,
        on_progress: tokio::sync::watch::Sender<FlashProgress>,
    ) -> FlashResult<()>
    where
        T::Handle: RawWriteHandle,
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
        // Calculate buffer size aligned to both page size and sector size
        let page_size = rustix::param::page_size();
        let sector_size = handle_target_write_to.sector_size();
        // Find smallest multiple larger than or equal to 1024 that's aligned to max(page_size, sector_size)
        let alignment = page_size.max(sector_size);
        let mut buffer_size = 1024;
        while buffer_size % alignment != 0 {
            buffer_size += 1;
        }

        let buffer = PageAlignedBuffer::new(buffer_size / page_size).expect("error");
        // SAFETY: Buffer is page-aligned and size is multiple of page size
        let mut buffer_slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer_size) };
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
                handle_target_write_to
                    .write_at(offset, &buffer_slice)
                    .await?;
                offset += read_back as u64;
                break;
            } else {
                info!("Read: 2");
                handle_target_write_to
                    .write_at(offset, &buffer_slice)
                    .await?;
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
        handle_target_write_to
            .seek(std::io::SeekFrom::Start(0))
            .await?;
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
    async fn verify(
        &self,
        handle: &mut T::Handle,
        source: AsyncImageSourceFile,
        written_bytes: u64,
        send_progress: tokio::sync::watch::Sender<FlashProgress>,
    ) -> FlashResult<tokio::sync::watch::Sender<FlashProgress>>
    where
        T::Handle: RawWriteHandle,
    {
        use sha2::{
            Digest,
            Sha256,
        };
        let mut timer = std::time::Instant::now();
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        let mut tmp_counter: u64 = 0;
        // Calculate buffer size aligned to both page size and sector size
        let page_size = rustix::param::page_size();
        let sector_size = handle.sector_size();
        // Find smallest multiple larger than or equal to 1024 that's aligned to max(page_size, sector_size)
        let alignment = page_size.max(sector_size);
        let mut buffer_size = 1024;
        while buffer_size % alignment != 0 {
            buffer_size += 1;
        }

        let buffer = PageAlignedBuffer::new(buffer_size / page_size).expect("error");
        // SAFETY: Buffer is page-aligned and size is multiple of page size
        let buffer_slice = unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer_size) };
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
}
