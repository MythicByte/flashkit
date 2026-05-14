use flashkit::flash;
use tracing::{
    Level,
    info,
};
use tracing_subscriber::{
    FmtSubscriber,
    fmt::format::FmtSpan,
};

fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Tracing Subscriber failed to setup");
    info!("Startup");
    let flasher = flash();
    let devices = flasher.list_devices();
    dbg!(devices);
}
