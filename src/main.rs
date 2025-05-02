use clap::Parser;
use std::net::SocketAddrV4;
use tracing_subscriber::prelude::*;

#[derive(clap::Parser)]
struct Options {
    #[arg(short, long, conflicts_with = "host", default_value_t = 9986)]
    port: u16,
    host: Option<SocketAddrV4>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let opts = Options::parse();

    if let Some(addr) = opts.host {
        yclip::run_satellite(addr).await?;
    } else {
        yclip::run_host(opts.port).await?;
    }

    Ok(())
}

fn init_logging() {
    let subscriber = tracing_subscriber::registry();

    // Respect RUST_LOG, falling back to INFO
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy();
    let subscriber = subscriber.with(filter);

    let layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let subscriber = subscriber.with(layer);
    subscriber.init();
}
