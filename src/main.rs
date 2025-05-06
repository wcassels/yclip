use clap::Parser;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let opts = Options::parse();

    let res = if let Some(addr) = opts.host {
        yclip::run_satellite(addr, opts.refresh_interval).await
    } else {
        yclip::run_host(opts.port, opts.refresh_interval).await
    };

    if let Err(e) = res {
        tracing::error!("yclip error: {e}");
        std::process::exit(1);
    }

    Ok(())
}

fn init_logging() {
    use tracing_subscriber::prelude::*;
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

#[derive(clap::Parser)]
#[command(version)]
struct Options {
    /// Listen for incoming connections on this port
    #[arg(short, long, conflicts_with = "host", default_value_t = yclip::DEFAULT_PORT)]
    port: u16,
    /// Local clipboard check interval (ms)
    #[arg(short, long, value_parser = duration_from_millis, default_value = "200")]
    refresh_interval: Duration,
    host: Option<yclip::HostAddr>,
}

fn duration_from_millis(s: &str) -> Result<Duration, <u64 as std::str::FromStr>::Err> {
    s.parse().map(Duration::from_millis)
}
