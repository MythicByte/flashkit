//!
use flashkit::traits::RawWriteHandle;

use std::{
    io::{
        Seek,
        SeekFrom,
        Write,
    },
    thread,
};

use flashkit::{
    data_types::{
        BlockDevice,
        ImageSourceFile,
    },
    error::FlashResult,
    traits::{
        DeviceEjector,
        DeviceUnmounter,
        DeviceWriter,
        Flasher,
    },
};
use sha2::{
    Digest,
    Sha256,
};
use tempfile::tempfile;
#[allow(missing_docs)]
struct TestWriteHandle {
    file: std::fs::File,
}

impl RawWriteHandle for TestWriteHandle {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
        use std::os::unix::fs::FileExt;
        self.file
            .write_all_at(buf, offset)
            .map_err(flashkit::error::FlashError::Io)
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()> {
        use std::os::unix::fs::FileExt;
        self.file
            .read_exact_at(buf, offset)
            .map_err(flashkit::error::FlashError::Io)
    }
    fn flush_to_disk(&mut self) -> FlashResult<()> {
        self.file
            .sync_all()
            .map_err(flashkit::error::FlashError::Io)
    }
    fn sector_size(&self) -> usize {
        512
    }
    fn size_bytes(&self) -> FlashResult<u64> {
        Ok(u64::MAX)
    }
}

impl std::io::Write for TestWriteHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

struct TestDeviceWriter {
    file: std::fs::File,
}

impl DeviceWriter for TestDeviceWriter {
    type Handle = TestWriteHandle;
    fn open_for_writing(&self, _device: &BlockDevice) -> FlashResult<Self::Handle> {
        Ok(TestWriteHandle {
            // Clone the file handle so the writer owns it
            file: self.file.try_clone().expect("clone failed"),
        })
    }
}

struct TestUnmounter;
impl DeviceUnmounter for TestUnmounter {
    fn unmount_all(&self, _device: &BlockDevice) -> FlashResult<()> {
        Ok(())
    }
    fn check_is_fully_unmounted(&self, _device: &BlockDevice) -> FlashResult<bool> {
        Ok(true)
    }
}

struct TestEjector;
impl DeviceEjector for TestEjector {
    fn eject(&self, _device: &BlockDevice) -> FlashResult<()> {
        Ok(())
    }
}

struct TestEnumerator;
impl flashkit::traits::DeviceEnumerator for TestEnumerator {
    fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
        Ok(vec![])
    }
}

// --- Test ---

#[test]
fn test_block_flash() {
    let mut input = tempfile().unwrap();
    let message = "sd fjsdjklfsdjfkjsdfshdjkdfsjhjsdlfkjsdfhhsldfhlhsjdfkhjLJHKSDJHKF jh sklhDFjjsFl hsJDF sld DL fkdS DF hjSDHJFsLKJDFhsuifuejsFhlsdKFhshJDFj sd s fs [s dsdf sdf sJHFhsHfsDFHJjhwueirysueir23498703273498dsahjfhgds i ysd]";
    let hash = Sha256::digest(message.as_bytes());
    let size = message.len();

    input.write_all(message.as_bytes()).unwrap();
    input.seek(SeekFrom::Start(0)).unwrap();

    // Output: plain tempfile — handed directly to TestDeviceWriter
    let output = tempfile().unwrap();

    // Build a Flasher with test doubles instead of real Linux drivers
    let flasher = Flasher::new(
        TestEnumerator,
        TestUnmounter,
        TestDeviceWriter { file: output },
        TestEjector,
        4096, // chunk_size
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let image = ImageSourceFile::new(input, size as u64, Some(hash.0));

    // BlockDevice path doesn't matter — TestDeviceWriter ignores it
    let block_device = BlockDevice::new(
        std::path::PathBuf::from("/dev/null"),
        "test".to_string(),
        10_000_000_000,
        true,
        None,
        512,
    );

    thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            println!("{:?}", msg);
        }
    });

    flasher.block_flash(image, &block_device, tx).unwrap();
}
