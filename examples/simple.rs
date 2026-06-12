//! simple devices get and there size
use flashkit::flash;
use tokio_stream::StreamExt;
use tracing::{
    Level,
    info,
};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Tracing Subscriber failed to setup");
    info!("Startup");
    let flasher = flash().await.expect("Flasher failed");
    let mut device_stream = flasher.watch_devices().await.expect("watch devices failed");

    info!("Watcher active. Awaiting hotplug events...");

    // 2. Loop continuously over the incoming events yielded by the active stream
    while let Some(event) = device_stream.next().await {
        match event {
            flashkit::data_types::DeviceEvent::Added(device) => {
                info!("Device Attached: {} ({:?})", device.name, device.path);
            }
            flashkit::data_types::DeviceEvent::Removed(path) => {
                info!("Device Detached: {:?}", path);
            }
        }
    }
}
