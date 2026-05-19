// //!
// use flashkit::{
//     data_types::BlockDevice,
//     error::FlashResult,
//     traits::{
//         DeviceEjector,
//         DeviceUnmounter,
//         DeviceWriter,
//         Flasher,
//         ImageSource,
//         RawWriteHandle,
//     },
// };
// use sha2::{
//     Digest,
//     Sha256,
// };
// use std::{
//     io::{
//         Read,
//         Write,
//     },
//     pin::Pin,
//     task::{
//         Context,
//         Poll,
//     },
// };
// use tempfile::tempfile;
// use tokio::io::{
//     AsyncBufRead,
//     AsyncRead,
//     AsyncSeek,
//     AsyncWrite,
// };

// #[allow(missing_docs)]
// struct TestWriteHandle {
//     file: std::fs::File,
// }

// impl RawWriteHandle for TestWriteHandle {
//     fn write_at(&mut self, offset: u64, buf: &[u8]) -> FlashResult<()> {
//         use std::os::unix::fs::FileExt;
//         self.file
//             .write_all_at(buf, offset)
//             .map_err(flashkit::error::FlashError::Io)
//     }
//     fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> FlashResult<()> {
//         use std::os::unix::fs::FileExt;
//         self.file
//             .read_exact_at(buf, offset)
//             .map_err(flashkit::error::FlashError::Io)
//     }
//     fn flush_to_disk(&mut self) -> FlashResult<()> {
//         self.file
//             .sync_all()
//             .map_err(flashkit::error::FlashError::Io)
//     }
//     fn sector_size(&self) -> usize {
//         512
//     }
//     fn size_bytes(&self) -> FlashResult<u64> {
//         Ok(u64::MAX)
//     }
// }

// impl std::io::Write for TestWriteHandle {
//     fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
//         self.file.write(buf)
//     }
//     fn flush(&mut self) -> std::io::Result<()> {
//         self.file.flush()
//     }
// }

// impl AsyncWrite for TestWriteHandle {
//     fn poll_write(
//         self: Pin<&mut Self>,
//         _cx: &mut Context<'_>,
//         buf: &[u8],
//     ) -> Poll<std::io::Result<usize>> {
//         Poll::Ready(self.get_mut().file.write(buf))
//     }
//     fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
//         Poll::Ready(self.get_mut().file.flush())
//     }
//     fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
//         self.poll_flush(cx)
//     }
// }

// impl AsyncRead for TestWriteHandle {
//     fn poll_read(
//         self: Pin<&mut Self>,
//         _cx: &mut Context<'_>,
//         buf: &mut tokio::io::ReadBuf<'_>,
//     ) -> Poll<std::io::Result<()>> {
//         let this = self.get_mut();
//         let unfilled = buf.initialize_unfilled();
//         match this.file.read(unfilled) {
//             Ok(n) => {
//                 buf.advance(n);
//                 Poll::Ready(Ok(()))
//             }
//             Err(e) => Poll::Ready(Err(e)),
//         }
//     }
// }

// struct TestDeviceWriter {
//     file: std::fs::File,
// }
// impl DeviceWriter for TestDeviceWriter {
//     type Handle = TestWriteHandle;
//     fn open_for_writing(&self, _device: &BlockDevice) -> FlashResult<Self::Handle> {
//         Ok(TestWriteHandle {
//             file: self.file.try_clone().expect("clone failed"),
//         })
//     }
// }

// struct TestUnmounter;
// impl DeviceUnmounter for TestUnmounter {
//     fn unmount_all(&self, _device: &BlockDevice) -> FlashResult<()> {
//         Ok(())
//     }
//     fn check_is_fully_unmounted(&self, _device: &BlockDevice) -> FlashResult<bool> {
//         Ok(true)
//     }
// }

// struct TestEjector;
// impl DeviceEjector for TestEjector {
//     fn eject(&self, _device: &BlockDevice) -> FlashResult<()> {
//         Ok(())
//     }
// }

// struct TestEnumerator;
// impl flashkit::traits::DeviceEnumerator for TestEnumerator {
//     fn list_devices(&self) -> FlashResult<Vec<BlockDevice>> {
//         Ok(vec![])
//     }
// }

// #[derive(Debug)]
// struct TestAsyncSource {
//     data: Vec<u8>,
//     pos: usize,
//     size: u64,
//     hash: Option<[u8; 32]>,
// }

// impl TestAsyncSource {
//     fn new(data: Vec<u8>, hash: Option<[u8; 32]>) -> Self {
//         let size = data.len() as u64;
//         Self {
//             data,
//             pos: 0,
//             size,
//             hash,
//         }
//     }
// }

// impl ImageSource for TestAsyncSource {
//     fn uncompressed_size(&self) -> u64 {
//         self.size
//     }
//     fn read_chunk(&mut self, buf: &mut [u8]) -> FlashResult<usize> {
//         let n = std::io::Read::read(&mut &self.data[self.pos..], buf)
//             .map_err(flashkit::error::FlashError::Io)?;
//         self.pos += n;
//         Ok(n)
//     }
//     fn expected_hash(&self) -> Option<[u8; 32]> {
//         self.hash
//     }
// }

// impl AsyncRead for TestAsyncSource {
//     fn poll_read(
//         self: Pin<&mut Self>,
//         _cx: &mut Context<'_>,
//         buf: &mut tokio::io::ReadBuf<'_>,
//     ) -> Poll<std::io::Result<()>> {
//         let this = self.get_mut();
//         let available = &this.data[this.pos..];
//         let to_read = buf.remaining().min(available.len());
//         buf.put_slice(&available[..to_read]);
//         this.pos += to_read;
//         Poll::Ready(Ok(()))
//     }
// }

// impl AsyncBufRead for TestAsyncSource {
//     fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<&[u8]>> {
//         let this = self.get_mut();
//         Poll::Ready(Ok(&this.data[this.pos..]))
//     }

//     fn consume(self: Pin<&mut Self>, amt: usize) {
//         self.get_mut().pos += amt;
//     }
// }
// impl AsyncSeek for TestWriteHandle {
//     fn start_seek(self: Pin<&mut Self>, position: std::io::SeekFrom) -> std::io::Result<()> {
//         use std::io::Seek;
//         // Access the underlying synchronous std::fs::File and seek it
//         let this = self.get_mut();
//         this.file.seek(position).map(|_| ())
//     }

//     fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<u64>> {
//         use std::io::Seek;
//         let this = self.get_mut();
//         // Return the current cursor position to complete the contract
//         Poll::Ready(this.file.stream_position())
//     }
// }

// #[tokio::test]
// async fn test_async_flash() {
//     let message = "sd fjsdjklfsdjfkjsdfshdjkdfsjhjsdlfkjsdfhhsldfhlhsjdfkhjLJHKSDJHKF jh sklhDFjjsFl hsJDF sld DL fkdS DF hjSDHJFsLKJDFhsuifuejsFhlsdKFhshJDFj sd s fs [s dsdf sdf sJHFhsHfsDFHJjhwueirysueir23498703273498dsahjfhgds i ysd]";
//     let hash = Sha256::digest(message.as_bytes());

//     let output = tempfile().unwrap();
//     let flasher = Flasher::new(
//         TestEnumerator,
//         TestUnmounter,
//         TestDeviceWriter { file: output },
//         TestEjector,
//         4096,
//     );

//     let (tx, mut rx) = tokio::sync::mpsc::channel(100);
//     let image = TestAsyncSource::new(message.as_bytes().to_vec(), Some(hash.0));

//     let block_device = BlockDevice::new(
//         std::path::PathBuf::from("/dev/null"),
//         "test".into(),
//         10_000_000_000,
//         true,
//         None,
//         512,
//     );

//     tokio::spawn(async move {
//         while let Some(msg) = rx.recv().await {
//             println!("{:?}", msg);
//         }
//     });

//     flasher.flash(image, &block_device, tx).await.unwrap();
// }
