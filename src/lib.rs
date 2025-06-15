use arboard::ImageData;
pub use clipboard::Clipboard;
use clipboard::{Board, ClipboardChange};
pub use connection::Connection;
use connection::ReadResult;
pub use secure::{Noise, Secret};
use std::{
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

pub async fn run_satellite(
    addr: SocketAddr,
    refresh_interval: Duration,
    password: Option<String>,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;

    let password = password.unwrap_or_default();
    let Some(noise) = secure::Noise::satellite(&mut stream, password.as_str()).await? else {
        return Ok(());
    };
    info!("Connected to clipboard on {addr}");
    let connection = Connection::new(stream, addr.to_string(), noise);

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher::<arboard::Clipboard>(Arc::clone(&notify), refresh_interval);
    watch_remote::<_, arboard::Clipboard>(connection, notify).await?; // Blocks
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
        let Some(noise) = Noise::host(&mut stream, &peer_addr, &secret).await? else {
            let _ = stream.shutdown().await;
            continue;
        };

        info!("New client {peer_addr} connected");
        let connection = Connection::new(stream, peer_addr.to_string(), noise);
        let notify = Arc::clone(&notify);

        tokio::task::spawn(async move {
            match watch_remote::<_, arboard::Clipboard>(connection, notify).await {
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
    notify: Arc<Notify>,
) -> anyhow::Result<()>
where
    B: Board,
{
    let mut board = B::new()?;
    loop {
        trace!("{}: selecting...", connection.peer_addr());
        tokio::select! {
            _notified = notify.notified() => {
                trace!("Notified of clipboard change");
                // Someone has updated the clipboard. Send it to our client.
                let lock = clipboard::LATEST_CHANGE.read().await;
                let new_clip = lock.as_ref().ok_or_else(|| anyhow::anyhow!("logic bug: we were notified but the clipboard was empty"))?;
                // We're holding a read lock while we write the bytes into
                // the buffer (just so we can log them afterwards!)
                connection.send(new_clip).await?;
                debug!("Sent {new_clip} to {}", connection.peer_addr());
                drop(lock);
            },
            result = connection.read() => {
                match result? {
                    ReadResult::Done(change) => {
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
                    }
                    ReadResult::Incomplete => continue,
                    ReadResult::Eof => return Ok(()),
                }
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
