use arboard::Clipboard;
use std::{
    ffi::{CString, FromVecWithNulError},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Notify, RwLock},
};
use tracing::*;

pub const DEFAULT_PORT: u16 = 9986;

#[derive(Clone, Debug)]
pub struct HostAddr(SocketAddr);

impl HostAddr {
    async fn connect(&self) -> anyhow::Result<TcpStream> {
        let stream = TcpStream::connect(&self.0).await?;
        stream.set_nodelay(true)?;
        info!("Connected to clipboard on {}", self.0);
        Ok(stream)
    }

    pub fn into_inner(self) -> SocketAddr {
        self.0
    }
}

impl FromStr for HostAddr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let target = if s.contains(':') {
            s.to_string()
        } else {
            format!("{s}:{DEFAULT_PORT}")
        };
        // Only log the "Resolved {} to {}" message if the argument wasn't already a parseable socketaddr
        target
            .parse()
            .or_else(|_| {
                target
                    .to_socket_addrs()?
                    .next()
                    .inspect(|addr| info!("Resolved {target} to {addr}"))
                    .ok_or_else(|| anyhow::anyhow!("Couldn't resolve {target} to a socket address"))
            })
            .map(Self)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EncodedClipboard(CString);

impl EncodedClipboard {
    pub fn decode(&self) -> anyhow::Result<String> {
        let utf8 = cobs::decode_vec(self.0.as_bytes())?;
        Ok(String::from_utf8(utf8)?)
    }

    pub fn encode(clipboard: String) -> Option<Self> {
        if clipboard.is_empty() {
            return None;
        }
        let mut bytes = cobs::encode_vec(clipboard.as_bytes());
        bytes.push(0);
        Some(Self(CString::from_vec_with_nul(bytes).unwrap()))
    }

    pub fn as_bytes_with_nul(&self) -> &[u8] {
        self.0.as_bytes_with_nul()
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, FromVecWithNulError> {
        CString::from_vec_with_nul(bytes).map(Self)
    }
}

static CLIPBOARD: RwLock<Option<EncodedClipboard>> = RwLock::const_new(None);

pub async fn run_satellite(addr: HostAddr, refresh_interval: Duration) -> anyhow::Result<()> {
    let stream = addr.connect().await?;
    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);
    watch_remote(stream, notify, addr.into_inner()).await?; // Blocks
    info!("Host disconnected");

    Ok(())
}

pub async fn run_host(port: u16, refresh_interval: Duration) -> anyhow::Result<()> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
            .await?;

    info!("Listening for connections on port {port}");

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

    let mut get_text = || -> anyhow::Result<Option<EncodedClipboard>> {
        use arboard::Error;
        match ctx.get_text() {
            // Handles empty clipboard
            Ok(s) => Ok(EncodedClipboard::encode(s)),
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
    let (read, write) = stream.split();
    let mut writer = tokio::io::BufWriter::new(write);
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
                // We're holding a read lock while we write the bytes into
                // the buffer. Probably not a big deal?
                writer.write_all(new_clip.as_bytes_with_nul()).await?;
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
                clip_ctx.set_text(decoded)?;
                *CLIPBOARD.write().await = Some(new_clip);

                // We're not a waiter, so we won't get woken up by our own update later
                notify.notify_waiters();
            },
        }
    }
}
