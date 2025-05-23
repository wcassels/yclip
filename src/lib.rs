use arboard::Clipboard;
use std::{
    ffi::{CString, FromVecWithNulError},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{Notify, RwLock},
};
use tracing::*;

pub const UNSPECIFIED: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

#[derive(Debug, PartialEq, Eq)]
struct EncodedClipboard(CString);

impl EncodedClipboard {
    pub fn decode(&self) -> anyhow::Result<String> {
        let utf8 = cobs::decode_vec(self.0.as_bytes())?;
        Ok(String::from_utf8(utf8)?)
    }

    // Assumes non-empty clipboard
    pub fn encode(clipboard: &str) -> Self {
        let mut bytes = cobs::encode_vec(clipboard.as_bytes());
        bytes.push(0);
        Self(CString::from_vec_with_nul(bytes).unwrap())
    }

    pub fn as_bytes_with_nul(&self) -> &[u8] {
        self.0.as_bytes_with_nul()
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, FromVecWithNulError> {
        CString::from_vec_with_nul(bytes).map(Self)
    }
}

static CLIPBOARD: RwLock<Option<EncodedClipboard>> = RwLock::const_new(None);

pub async fn run_satellite(addr: SocketAddr, refresh_interval: Duration) -> anyhow::Result<()> {
    let stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;
    info!("Connected to clipboard on {addr}");

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);
    watch_remote(stream, notify, addr).await?; // Blocks
    info!("Host disconnected");

    Ok(())
}

pub async fn run_host(refresh_interval: Duration) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(UNSPECIFIED).await?;

    let local_addr = {
        let socket = UdpSocket::bind(UNSPECIFIED).await?;
        socket.connect("1.1.1.1:1").await?;
        socket.local_addr()?.ip()
    };

    info!(
        "Run `yclip {local_addr}:{}` to connect to this clipboard",
        listener.local_addr()?.port()
    );

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);

    loop {
        let (stream, client_addr) = listener.accept().await?;
        stream.set_nodelay(true)?;
        info!("New client {client_addr} connected");
        let notify = Arc::clone(&notify);

        tokio::task::spawn(async move {
            match watch_remote(stream, notify, client_addr).await {
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

async fn watch_remote(
    mut stream: TcpStream,
    notify: Arc<Notify>,
    remote_addr: SocketAddr,
) -> anyhow::Result<()> {
    let (read, mut writer) = stream.split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut clip_ctx = Clipboard::new()?;

    let mut incoming_bytes = Vec::new();
    loop {
        trace!("{remote_addr}: selecting... (current bytes: {incoming_bytes:?})");
        tokio::select! {
            _notified = notify.notified() => {
                trace!("Notified of clipboard change");
                // Someone has updated the clipboard. Send it to our client.
                let lock = CLIPBOARD.read().await;
                let new_clip = lock.as_ref().ok_or_else(|| anyhow::anyhow!("logic bug: we were notified but the clipboard was empty"))?;
                let encoded = EncodedClipboard::encode(new_clip);
                // We're holding a read lock while we write the bytes into
                // the buffer (just so we can log them afterwards!)
                writer.write_all(encoded.as_bytes_with_nul()).await?;
                debug!("Sent {new_clip:?} to {remote_addr}");
                drop(lock);
                writer.flush().await?;
            },
            bytes_read = reader.read_until(0, &mut incoming_bytes) => {
                let bytes_read = bytes_read?;
                if bytes_read == 0 {
                    // EOF
                    return Ok(());
                } else if incoming_bytes.last().copied() != Some(0) {
                    // TODO: Actually implement handshake/encryption so can worry less about malicious messages
                    warn!(%remote_addr, "Mid-way through receiving incomplete message. This read: {bytes_read}, total: {} bytes", incoming_bytes.len());
                    continue;
                }

                let completed_message = incoming_bytes.clone();
                incoming_bytes.clear();

                let new_clip = match EncodedClipboard::from_bytes(completed_message) {
                    Ok(c) => {
                        debug!("Received {c:?} from {remote_addr}");
                        c
                    }
                    Err(e) => {
                        error!("Incoming clipboard from {remote_addr} was malformed: {e:?}");
                        continue;
                    }
                };

                let decoded = match new_clip.decode() {
                    Ok(d) => d,
                    Err(e) => {
                        error!("Couldn't decode incoming clipboard. What happened? {e}");
                        continue;
                    }
                };
                clip_ctx.set_text(decoded.as_str())?;
                *CLIPBOARD.write().await = Some(decoded);

                // We're not a waiter, so we won't get woken up by our own update later
                notify.notify_waiters();
            },
        }
    }
}
