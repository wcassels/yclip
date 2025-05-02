use clap::Parser;
use std::net::SocketAddrV4;

#[derive(clap::Parser)]
struct Options {
    host: Option<SocketAddrV4>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Options::parse();

    if let Some(addr) = opts.host {
        yclip::run_satellite(addr).await?;
    } else {
        yclip::run_host().await?;
    }

    Ok(())
}
