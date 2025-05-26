use clap::Parser;
use std::{net::SocketAddr, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Options::parse();
    init_logging(&opts)?;

    let res = if let Some(addr) = opts.host {
        yclip::run_satellite(addr, opts.refresh_interval, opts.secret).await
    } else {
        yclip::run_host(opts.refresh_interval, opts.secret).await
    };

    if let Err(e) = res {
        tracing::error!("yclip error: {e:?}");
        std::process::exit(1);
    }

    Ok(())
}

fn init_logging(opts: &Options) -> anyhow::Result<()> {
    use tracing::Level;
    use tracing_subscriber::prelude::*;

    let level = match opts.verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        2 => Level::TRACE,
        _ => anyhow::bail!("Maximum supported verbosity is TRACE (-vv)"),
    };

    let subscriber = tracing_subscriber::registry();
    let filter = tracing_subscriber::filter::LevelFilter::from_level(level);
    let subscriber = subscriber.with(filter);

    let layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let subscriber = subscriber.with(layer);
    subscriber.init();
    Ok(())
}

#[derive(clap::Parser)]
#[command(version)]
struct Options {
    /// Local clipboard check interval (ms)
    #[arg(short, long, value_parser = duration_from_millis, default_value = "200")]
    refresh_interval: Duration,
    /// Connect to the yclip server running on this host
    host: Option<SocketAddr>,
    #[arg(short, long, required(cfg!(feature = "force-secure")))]
    /// Encrypt clipboards using this secret. Compile with the "force-secure" feature enabled
    /// to make encryption mandatory.
    secret: Option<String>,
    /// Increase verbosity (defaults to INFO and above)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn duration_from_millis(s: &str) -> Result<Duration, <u64 as std::str::FromStr>::Err> {
    s.parse().map(Duration::from_millis)
}
