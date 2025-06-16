use arboard::ImageData;
pub use clipboard::Clipboard;
use clipboard::{Board, ClipboardChange};
pub use connection::Connection;
pub use secure::{Noise, Secret};
use std::{
    fmt::Display,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    net::{TcpStream, UdpSocket},
    sync::Notify,
};
pub use tracing;
use tracing::*;

mod clipboard;
mod connection;
mod secure;

#[cfg(any(test, fuzzing))]
pub mod test;

const UNSPECIFIED: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run_satellite(
    addr: SocketAddr,
    refresh_interval: Duration,
    password: Option<String>,
) -> anyhow::Result<()> {
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
        Ok(s) => s
            .and_then(|x| {
                x.set_nodelay(true)?;
                Ok(x)
            })
            .into_anyhow(format_args!("Failed to connect to {addr}"))?,
        Err(_elapsed) => anyhow::bail!("Timed out connecting to {addr}"),
    };

    let password = password.unwrap_or_default();
    let noise = secure::Noise::satellite(&mut stream, password.as_str()).await?;
    info!("Connected to clipboard on {addr}");
    let connection = Connection::new(stream, addr.to_string(), noise);

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher::<arboard::Clipboard>(Arc::clone(&notify), refresh_interval);
    let board = <arboard::Clipboard as Board>::new()?;
    watch_remote(connection, board, notify).await?; // Blocks
    info!("Host disconnected");

    Ok(())
}

pub async fn run_host(refresh_interval: Duration, password: Option<String>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(UNSPECIFIED).await?;

    let local_addr = {
        let socket = UdpSocket::bind(UNSPECIFIED).await?;
        socket.connect("1.1.1.1:1").await?;
        socket.local_addr()?.ip()
    };

    info!(
        "Run `yclip {local_addr}:{}{}` to connect to this clipboard",
        listener.local_addr()?.port(),
        password
            .as_ref()
            .map(|s| format!(" -p {s}"))
            .unwrap_or_default()
    );

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher::<arboard::Clipboard>(Arc::clone(&notify), refresh_interval);
    let secret = Secret::new(password.unwrap_or_default().as_str(), None);

    loop {
        let (mut stream, peer_addr) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let noise = match Noise::host(&mut stream, &peer_addr, &secret).await {
            Ok(n) => n,
            Err(e) => {
                error!("{e}");
                let _ = stream.shutdown().await;
                continue;
            }
        };

        info!("New client {peer_addr} connected");
        let connection = Connection::new(stream, peer_addr.to_string(), noise);
        let notify = Arc::clone(&notify);
        let board = <arboard::Clipboard as Board>::new()?;
        tokio::task::spawn(async move {
            match watch_remote(connection, board, notify).await {
                Ok(()) => info!("Client {peer_addr} disconnected"),
                Err(e) => error!("Remote watcher exited with an error: {e}"),
            }
        });
    }
}

fn spawn_local_watcher<B: Board>(notify: Arc<Notify>, poll_interval: Duration) {
    tokio::task::spawn(async move {
        if let Err(e) = watch_local::<B>(notify, poll_interval).await {
            error!("Local watcher exited with an error: {e}");
        }
    });
}

pub async fn watch_local<B: Board>(
    notify: Arc<Notify>,
    poll_interval: Duration,
) -> anyhow::Result<()> {
    let mut board = B::new()?;
    let latest_text = board.get_text()?.map(ClipboardChange::Text);
    let latest_image = board.get_image()?.map(ClipboardChange::Image);
    clipboard::store_hash(latest_text.as_ref()).await;
    clipboard::store_hash(latest_image.as_ref()).await;
    drop(board);

    let mut clipboard = Clipboard::<B>::new(poll_interval)?;
    loop {
        let change = clipboard.listen_for_change().await?;
        *clipboard::LATEST_CHANGE.write().await = Some(change);
        notify.notify_waiters();
    }
}

pub async fn watch_remote<T: AsyncRead + AsyncWrite, B: Board>(
    mut connection: Connection<T>,
    mut board: B,
    notify: Arc<Notify>,
) -> anyhow::Result<()> {
    let peer_addr = &connection.peer_addr().to_string();
    loop {
        trace!("{}: selecting...", peer_addr);
        tokio::select! {
            _notified = notify.notified() => {
                trace!("Notified of clipboard change");
                // Someone has updated the clipboard. Send it to our client.
                let lock = clipboard::LATEST_CHANGE.read().await;
                let change = lock.as_ref().expect("logic bug: we were notified but the clipboard was empty");

                // We're holding a read lock while we write the bytes into
                // the buffer (just so we can log them afterwards!)
                connection.send(change, peer_addr).await?;
                drop(lock);
            },
            result = connection.read() => {
                let Some(change) = result? else {
                    return Ok(());
                };
                debug!("Received new clipboard: {change}");
                clipboard::store_hash(Some(&change)).await;
                match change {
                    ClipboardChange::Text(s) => {
                        board.set_text(s.as_str());
                    },
                    ClipboardChange::Image(x) => {
                        board.set_image(ImageData {
                            height: x.height,
                            width: x.width,
                            // Might as well avoid copying this
                            bytes: std::borrow::Cow::Borrowed(x.bytes.as_ref()),
                        });
                    },
                }

                // We're not a waiter, so we won't get woken up by our own update later
                notify.notify_waiters();
            },
        }
    }
}

pub fn init_logging(level: Level) -> anyhow::Result<()> {
    use tracing_subscriber::prelude::*;

    let subscriber = tracing_subscriber::registry();
    let filter = tracing_subscriber::filter::LevelFilter::from_level(level);
    let subscriber = subscriber.with(filter);

    let layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let subscriber = subscriber.with(layer);
    subscriber.init();
    Ok(())
}

pub trait IntoAnyhow<T> {
    fn into_anyhow(self, prefix: impl Display) -> anyhow::Result<T>;
}

impl<T, E: std::error::Error> IntoAnyhow<T> for Result<T, E> {
    fn into_anyhow(self, prefix: impl Display) -> anyhow::Result<T> {
        match self {
            Ok(x) => Ok(x),
            Err(e) => Err(anyhow::anyhow!("{prefix}: {e}")),
        }
    }
}
