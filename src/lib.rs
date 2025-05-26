pub use clipboard::Clipboard;
pub use connection::Connection;
use connection::ReadResult;
pub use secure::{Noise, Secret};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, UdpSocket},
    sync::{Notify, RwLock},
};
pub use tracing;
use tracing::*;

mod clipboard;
mod connection;
mod secure;

const UNSPECIFIED: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

static CLIPBOARD: RwLock<Option<String>> = RwLock::const_new(None);

pub async fn run_satellite(
    addr: SocketAddr,
    refresh_interval: Duration,
    password: Option<String>,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;

    let password = password.unwrap_or_default();
    let noise = match secure::Noise::satellite(&mut stream, password.as_str()).await {
        Ok(n) => n,
        Err(e) if e.is_likely_password_mismatch() => {
            error!("Handshake failed. Are you sure the passwords match?");
            debug!("The actual error was {e:?}");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
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
        let noise = match Noise::host(&mut stream, &peer_addr, &secret).await {
            Ok(n) => n,
            Err(e) if e.is_likely_password_mismatch() => {
                error!("Failed handshake with {peer_addr} (likely password mismatch)");
                debug!("The actual error was {e:?}");
                continue;
            }
            Err(e) => return Err(e.into()),
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

fn spawn_local_watcher<C: Clipboard>(notify: Arc<Notify>, refresh_rate: Duration) {
    tokio::task::spawn(async move {
        if let Err(e) = watch_local::<C>(notify, refresh_rate).await {
            error!("Local watcher exited with an error: {e}");
        }
    });
}

pub async fn watch_local<C: Clipboard>(
    notify: Arc<Notify>,
    refresh_rate: Duration,
) -> anyhow::Result<()> {
    let mut clipboard = C::new()?;
    *CLIPBOARD.write().await = clipboard.get_text()?;

    loop {
        tokio::time::sleep(refresh_rate).await;
        let Some(new_text) = clipboard.get_text()? else {
            continue;
        };

        if CLIPBOARD.read().await.as_ref() != Some(&new_text) {
            debug!("Local clipboard change: {new_text}");
            *CLIPBOARD.write().await = Some(new_text);
            notify.notify_waiters();
        }
    }
}

pub async fn watch_remote<T: AsyncRead + AsyncWrite, C>(
    mut connection: Connection<T>,
    notify: Arc<Notify>,
) -> anyhow::Result<()>
where
    C: Clipboard,
{
    let mut clipboard = C::new()?;
    loop {
        trace!("{}: selecting...", connection.peer_addr());
        tokio::select! {
            _notified = notify.notified() => {
                trace!("Notified of clipboard change");
                // Someone has updated the clipboard. Send it to our client.
                let lock = CLIPBOARD.read().await;
                let new_clip = lock.as_ref().ok_or_else(|| anyhow::anyhow!("logic bug: we were notified but the clipboard was empty"))?;
                // We're holding a read lock while we write the bytes into
                // the buffer (just so we can log them afterwards!)
                connection.send(new_clip).await?;
                debug!("Sent {new_clip:?} to {}", connection.peer_addr());
                drop(lock);
            },
            result = connection.read() => {
                trace!("Read {:?}", result);
                match result? {
                    ReadResult::Done(s) => {
                        debug!("Received new clipboard text: {s}");
                        clipboard.set_text(s.as_str());
                        *CLIPBOARD.write().await = Some(s);

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
