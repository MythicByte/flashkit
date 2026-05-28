use sha2::{
    Digest,
    Sha256,
};
use tokio::io::{
    AsyncBufReadExt,
    AsyncReadExt,
    AsyncSeekExt,
};
use tracing::{
    error,
    info,
};

use crate::{
    aligned::PageAlignedBuffer,
    data_types::{
        AsyncImageSourceFile,
        BlockDevice,
        FlashPhase,
        FlashProgress,
        HashFailedWhen,
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
            .send(FlashProgress::create(FlashPhase::CheckingHash))
            .map_err(|_| FlashError::SendChannelError)?;

        let total_bytes_hash = &source_of_image.uncompressed_size();
        if let Some(hash_test_against) = &source_of_image.expected_hash() {
            let mut buff_reader = tokio::io::BufReader::new(source_of_image.file);
            let mut bytes_checked: u64 = 0;
            let mut bytes_since_last_report: u64 = 0;
            let mut timer = std::time::Instant::now();
            {
                buff_reader.fill_buf().await?;
                let mut hash_tester = Sha256::new();
                while !buff_reader.buffer().is_empty() {
                    let chunk_len = buff_reader.buffer().len();

                    hash_tester.update(&buff_reader.buffer());
                    buff_reader.consume(buff_reader.buffer().len());

                    bytes_checked += chunk_len as u64;
                    bytes_since_last_report += chunk_len as u64;
                    let elapsed = timer.elapsed().as_secs_f64();
                    if elapsed >= 1.00 {
                        on_progress
                            .send(FlashProgress {
                                bytes_written: bytes_checked,
                                total_bytes: *total_bytes_hash,
                                bytes_per_sec: bytes_since_last_report as f64 / elapsed,
                                phase: FlashPhase::CheckingHash,
                            })
                            .map_err(|_| FlashError::SendChannelError)?;

                        bytes_since_last_report = 0;
                        timer = std::time::Instant::now();
                    }
                    buff_reader.fill_buf().await?;
                }
                let hash_finalized = hash_tester.finalize();
                if hash_finalized.as_slice() != hash_test_against {
                    error!(
                        "correct: {:?} \nwrong: {:?}",
                        hash_finalized.as_slice(),
                        hash_test_against
                    );
                    return Err(FlashError::Sha256HashDoesNotMatch(
                        HashFailedWhen::FirstCheck,
                    ));
                }
            }
            source_of_image.file = buff_reader.into_inner();
        }
        // go the beginning again
        source_of_image
            .file
            .seek(std::io::SeekFrom::Start(0))
            .await?;

        on_progress
            .send(FlashProgress::create(FlashPhase::Unmounting))
            .map_err(|_| FlashError::SendChannelError)?;
        self.interface.unmount(device).await?;

        let mut handle_target_write_to = self.interface.open_for_writing(device).await?;
        let total_bytes = source_of_image.uncompressed_size();
        let file_size = handle_target_write_to.size_bytes()?;
        if source_of_image.uncompressed_size() > file_size {
            return Err(FlashError::TargetToSmall(
                source_of_image.uncompressed_size(),
                file_size,
            ));
        }
        Self::wipe_partition_table(&mut handle_target_write_to).await?;

        source_of_image
            .file
            .seek(std::io::SeekFrom::Start(0))
            .await?;
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

        let buffer = PageAlignedBuffer::new(1024).expect("Failed to allocate buffer"); // SAFETY: Buffer is page-aligned and size is multiple of page size
        let buffer_slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer.size()) };
        loop {
            let mut read_back = 0;
            while read_back < buffer.size() {
                let bytes_read = source_of_image
                    .file
                    .read(&mut buffer_slice[read_back..])
                    .await?;
                if bytes_read == 0 {
                    break; // 0 bytes means true End of File
                }
                read_back += bytes_read;
            }
            if read_back < buffer.size() {
                if read_back == 0 {
                    break;
                }
                if let Some(slice) = buffer_slice.get_mut(read_back..) {
                    for i in slice {
                        *i = 0;
                    }
                }
                handle_target_write_to.write_at(offset, buffer_slice)?;
                offset += read_back as u64;
                break;
            } else {
                handle_target_write_to.write_at(offset, buffer_slice)?;
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
        on_progress
            .send(FlashProgress::create(FlashPhase::Flushing))
            .map_err(|_| FlashError::SendChannelError)?;
        handle_target_write_to.flush_to_disk()?;
        handle_target_write_to.seek(std::io::SeekFrom::Start(0))?;
        on_progress
            .send(FlashProgress::create(FlashPhase::Verifying))
            .map_err(|_| FlashError::SendChannelError)?;
        info!("Verifyer started");
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
            .send(FlashProgress::create(FlashPhase::Done))
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

        let buffer = PageAlignedBuffer::new(1024).expect("Failed to allocate buffer");
        // SAFETY: Buffer is page-aligned and size is multiple of page size
        let buffer_slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer.size()) };
        loop {
            if offset >= written_bytes {
                break;
            }
            let read_back = handle.read_at(offset, buffer_slice)?;
            if read_back == 0 {
                break;
            }
            let valid_bytes = std::cmp::min(read_back, (written_bytes - offset) as usize);

            hasher.update(&buffer_slice[..valid_bytes]);
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
            && expected != actual.as_slice()
        {
            error!("Hash does not match");
            return Err(FlashError::Sha256HashDoesNotMatch(
                HashFailedWhen::VerificationCheck,
            ));
        }

        Ok(send_progress)
    }
}
impl<T> Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector,
{
    async fn wipe_partition_table(handle: &mut T::Handle) -> FlashResult<()>
    where
        T::Handle: RawWriteHandle,
    {
        // Deletes GPT backup header, the first is overwritten with the image later
        let buffer = PageAlignedBuffer::new(1).expect("Failed to allocate buffer"); // SAFETY: Buffer is page-aligned and size is multiple of page size
        let buffer_slice =
            unsafe { std::slice::from_raw_parts_mut(buffer.as_ptr(), buffer.size()) };
        buffer_slice.fill(0);
        // Wipe the backup GPT at the end of the disk
        let disk_size = handle.size_bytes()?;
        let end_offset = disk_size.saturating_sub(buffer.size() as u64);
        handle.write_at(end_offset, &buffer_slice)?;

        handle.flush_to_disk()?;
        handle.seek(std::io::SeekFrom::Start(0))?;

        Ok(())
    }
}
