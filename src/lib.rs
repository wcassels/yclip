use arboard::Clipboard;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream, UdpSocket,
    },
    sync::{Notify, RwLock},
};
use tracing::*;

mod secure;

const UNSPECIFIED: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer_addr: SocketAddr,
    noise: Option<secure::Noise>,
    read_buf: Vec<u8>,
    cobs_buf: Vec<u8>,
}

impl Connection {
    fn new(stream: TcpStream, peer_addr: SocketAddr, noise: Option<secure::Noise>) -> Self {
        let (read, writer) = stream.into_split();
        Self {
            reader: tokio::io::BufReader::new(read),
            writer,
            peer_addr,
            noise,
            read_buf: Vec::new(),
            cobs_buf: Vec::new(),
        }
    }

    /// Assumes `clipboard` is non-empty
    pub async fn send(&mut self, clipboard: &str) -> anyhow::Result<()> {
        let to_encode = if let Some(n) = self.noise.as_mut() {
            n.encode_message(clipboard)?
        } else {
            clipboard.as_bytes()
        };
        self.cobs_buf
            .resize(cobs::max_encoding_length(to_encode.len()), 0);
        let encoded_len = cobs::encode(to_encode, &mut self.cobs_buf);
        self.cobs_buf.truncate(encoded_len);
        self.cobs_buf.push(0);
        self.writer.write_all(self.cobs_buf.as_slice()).await?;
        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<ReadResult> {
        // This await is cancel-safe which means the whole method is... don't add any more awaits without
        // thinking about it
        let bytes_read = self.reader.read_until(0, &mut self.read_buf).await?;
        if bytes_read == 0 {
            return Ok(ReadResult::Eof);
        } else if self.read_buf.last().copied() != Some(0) {
            debug!(%self.peer_addr, "Mid-way through receiving incomplete message. This read: {bytes_read}, total: {} bytes", self.read_buf.len());
            return Ok(ReadResult::Incomplete);
        }

        self.read_buf.pop();
        let len = cobs::decode_in_place(&mut self.read_buf)?;
        self.read_buf.truncate(len);
        let to_decode = if let Some(n) = self.noise.as_mut() {
            n.decode_message(&self.read_buf)?
        } else {
            &self.read_buf
        };
        let result = String::from_utf8(to_decode.to_vec())?;
        self.read_buf.clear();

        Ok(ReadResult::Done(result))
    }

    pub fn peer_addr(&self) -> &SocketAddr {
        &self.peer_addr
    }
}

enum ReadResult {
    Eof,
    Incomplete,
    Done(String),
}

static CLIPBOARD: RwLock<Option<String>> = RwLock::const_new(None);

pub async fn run_satellite(
    addr: SocketAddr,
    refresh_interval: Duration,
    secret: Option<String>,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;

    let noise = if let Some(s) = secret {
        Some(secure::Noise::satellite(&mut stream, s).await?)
    } else {
        None
    };
    info!("Connected to clipboard on {addr}");
    let connection = Connection::new(stream, addr, noise);

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);
    watch_remote(connection, notify).await?; // Blocks
    info!("Host disconnected");

    Ok(())
}

pub async fn run_host(refresh_interval: Duration, secret: Option<String>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(UNSPECIFIED).await?;

    let local_addr = {
        let socket = UdpSocket::bind(UNSPECIFIED).await?;
        socket.connect("1.1.1.1:1").await?;
        socket.local_addr()?.ip()
    };

    info!(
        "Run `yclip {local_addr}:{}{}` to connect to this clipboard",
        listener.local_addr()?.port(),
        secret
            .as_ref()
            .map(|s| format!(" -s {s}"))
            .unwrap_or_default()
    );

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);
    let secret = secret.map(|s| secure::Secret::new(s, None));

    loop {
        let (mut stream, client_addr) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let noise = if let Some(s) = secret.as_ref() {
            Some(secure::Noise::host(&mut stream, &client_addr, s).await?)
        } else {
            None
        };
        info!(%client_addr, "New client connected");
        let connection = Connection::new(stream, client_addr, noise);
        let notify = Arc::clone(&notify);

        tokio::task::spawn(async move {
            match watch_remote(connection, notify).await {
                Ok(()) => info!("Client {client_addr} disconnected"),
                Err(e) => error!("Remote watcher exited: {e}"),
            }
        });
    }
}

fn spawn_local_watcher(notify: Arc<Notify>, refresh_rate: Duration) {
    tokio::task::spawn(async move {
        if let Err(e) = watch_local(notify, refresh_rate).await {
            error!("Local watcher exited: {e}");
        }
    });
}

async fn watch_local(notify: Arc<Notify>, refresh_rate: Duration) -> anyhow::Result<()> {
    let mut ctx = Clipboard::new()?;

    let mut get_text = || -> anyhow::Result<Option<String>> {
        use arboard::Error;
        match ctx.get_text() {
            // Don't broadcast clipboard reset!
            Ok(s) if s.is_empty() => Ok(None),
            Ok(s) => Ok(Some(s)),
            // Retry
            Err(Error::ContentNotAvailable | Error::ClipboardOccupied) => Ok(None),
            // For text, this means non-utf8 AFAICT.
            Err(Error::ConversionFailure) => {
                warn!("Clipboard conversion failure. Does the clipboard contain non-utf8 text?");
                Ok(None)
            }
            Err(Error::Unknown { description }) => {
                warn!("Error reading clipboard: {description}");
                // Retry? From a quick look at the library's source, these errors seem
                // transient
                Ok(None)
            }
            // No point retrying on this
            Err(e @ Error::ClipboardNotSupported) => Err(e.into()),
            // arboard::Error is marked as non_exhastive so we need a catch-all;
            // just fall over.
            Err(e) => Err(e.into()),
        }
    };

    *CLIPBOARD.write().await = get_text()?;

    loop {
        tokio::time::sleep(refresh_rate).await;
        let Some(new_clip) = get_text()? else {
            continue;
        };

        if CLIPBOARD.read().await.as_ref() != Some(&new_clip) {
            debug!("Local clipboard change: {new_clip:?}");
            *CLIPBOARD.write().await = Some(new_clip);
            notify.notify_waiters();
        }
    }
}

async fn watch_remote(mut connection: Connection, notify: Arc<Notify>) -> anyhow::Result<()> {
    let mut clip_ctx = Clipboard::new()?;

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
                match result? {
                    ReadResult::Done(s) => {
                        info!("Received new clipboard text: {s}");
                        clip_ctx.set_text(s.as_str())?;
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
