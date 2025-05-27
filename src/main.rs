use clap::Parser;
use std::{net::SocketAddr, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Options::parse();
    yclip::init_logging(match opts.verbose {
        0 => tracing::Level::INFO,
        1 => tracing::Level::DEBUG,
        2 => tracing::Level::TRACE,
        _ => anyhow::bail!("Maximum supported verbosity is TRACE (-vv)"),
    })?;

    let res = if let Some(addr) = opts.socket {
        yclip::run_satellite(addr, opts.refresh_interval, opts.password).await
    } else {
        yclip::run_host(opts.refresh_interval, opts.password).await
    };

    if let Err(e) = res {
        tracing::error!("yclip error: {e:?}");
        std::process::exit(1);
    }

    Ok(())
}

#[derive(clap::Parser)]
#[command(version)]
struct Options {
    /// Local clipboard check interval (ms)
    #[arg(short, long, value_parser = duration_from_millis, default_value = "200")]
    refresh_interval: Duration,
    /// Connect to the yclip server running on this socket address
    #[arg(value_parser = parse_socket_addr)]
    socket: Option<SocketAddr>,
    #[arg(short, long, required(cfg!(feature = "force-secure")))]
    /// Encrypt clipboards using this password. Compile with the "force-secure" feature enabled
    /// to make this mandatory.
    password: Option<String>,
    /// Increase verbosity (defaults to INFO and above)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn duration_from_millis(s: &str) -> Result<Duration, <u64 as std::str::FromStr>::Err> {
    s.parse().map(Duration::from_millis)
}

fn parse_socket_addr(s: &str) -> anyhow::Result<SocketAddr> {
    let mut addrs = std::net::ToSocketAddrs::to_socket_addrs(s)?;
    addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("Couldn't resolve {s} to a socket address"))
}
