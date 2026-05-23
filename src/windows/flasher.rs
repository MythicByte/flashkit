use crate::traits::{
    DeviceEjector,
    DeviceEnumerator,
    DeviceUnmounter,
    DeviceWriter,
    Flasher,
    FlasherGeneric,
};

impl<T> FlasherGeneric<T> for Flasher<T>
where
    T: DeviceEnumerator + DeviceUnmounter + DeviceWriter + DeviceEjector,
{
    async fn verify(
        &self,

        handle: &mut <T as DeviceWriter>::Handle,
        source: crate::data_types::AsyncImageSourceFile,
        written_bytes: u64,
        send_progress: tokio::sync::watch::Sender<crate::data_types::FlashProgress>,
    ) -> crate::error::FlashResult<tokio::sync::watch::Sender<crate::data_types::FlashProgress>>
    {
        todo!()
    }

    async fn flash(
        &self,
        source_of_image: crate::data_types::AsyncImageSourceFile,
        device: &crate::data_types::BlockDevice,
        on_progress: tokio::sync::watch::Sender<crate::data_types::FlashProgress>,
    ) -> crate::error::FlashResult<()> {
        todo!()
    }
}
