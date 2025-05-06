use arboard::Clipboard;
use std::{
    ffi::CString,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    ops::Deref,
    str::FromStr,
    sync::{Arc, LazyLock},
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
    async fn connect(self) -> anyhow::Result<TcpStream> {
        let stream = TcpStream::connect(&self.0).await?;
        stream.set_nodelay(true)?;
        info!("Connected to clipboard on {}", self.0);
        Ok(stream)
    }
}

impl FromStr for HostAddr {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let target = if s.contains(":") {
            s.to_string()
        } else {
            format!("{s}:{DEFAULT_PORT}")
        };
        let addr = target
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Couldn't resolve {target} to a socket address"))?;
        info!("Resolved {target} to {addr}");
        Ok(Self(addr))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct EncodedClipboard(CString);

impl EncodedClipboard {
    pub fn decode(&self) -> anyhow::Result<String> {
        let utf8 = cobs::decode_vec(self.0.as_bytes())?;
        Ok(String::from_utf8(utf8)?)
    }

    pub fn encode(clipboard: NonEmptyString) -> Self {
        let mut bytes = cobs::encode_vec(clipboard.as_bytes());
        bytes.push(0);
        Self(CString::from_vec_with_nul(bytes).unwrap())
    }

    pub fn as_bytes_with_nul(&self) -> &[u8] {
        self.0.as_bytes_with_nul()
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(CString::from_vec_with_nul(bytes).unwrap())
    }
}

#[derive(Debug)]
struct NonEmptyString(String);

impl NonEmptyString {
    pub fn new(s: String) -> Option<Self> {
        if s.is_empty() {
            None
        } else {
            Some(Self(s))
        }
    }
}

impl Deref for NonEmptyString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

static CLIPBOARD: LazyLock<RwLock<EncodedClipboard>> = LazyLock::new(|| {
    // TODO: These unwraps are getting a bit unwieldy...
    let clipboard = Clipboard::new().unwrap().get_text().unwrap();
    let non_empty = NonEmptyString::new(clipboard).unwrap();
    RwLock::const_new(EncodedClipboard::encode(non_empty))
});

pub async fn run_satellite(addr: HostAddr, refresh_interval: Duration) -> anyhow::Result<()> {
    let stream = addr.connect().await?;
    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);
    watch_remote(stream, notify).await?; // Blocks
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
            match watch_remote(stream, notify).await {
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
    let mut ctx = Clipboard::new().unwrap();

    LazyLock::force(&CLIPBOARD);

    let mut get_text = || -> anyhow::Result<Option<NonEmptyString>> {
        use arboard::Error;
        match ctx.get_text() {
            // We're not interested in broadcasting our clipboard if it gets reset
            // for whatever reason. It also makes the COBS decoder sad apparently
            Ok(s) => Ok(NonEmptyString::new(s)),
            // Including ClipboardOccupied here feels a bit hacky, but semantically
            // it's gonna mean we retry so :shrug:
            Err(Error::ContentNotAvailable | Error::ClipboardOccupied) => Ok(None),
            Err(e) => Err(e.into()),
        }
    };

    loop {
        tokio::time::sleep(refresh_rate).await;
        let Some(new_clip) = get_text()? else {
            continue;
        };

        let encoded = EncodedClipboard::encode(new_clip);
        if *CLIPBOARD.read().await != encoded {
            debug!("Local clipboard change: {encoded:?}");
            *CLIPBOARD.write().await = encoded;
            notify.notify_waiters();
        }
    }
}

async fn watch_remote(mut stream: TcpStream, notify: Arc<Notify>) -> anyhow::Result<()> {
    let remote_addr = stream.peer_addr()?;

    let (read, write) = stream.split();
    let mut writer = tokio::io::BufWriter::new(write);
    let mut reader = tokio::io::BufReader::new(read);
    let mut clip_ctx = Clipboard::new().unwrap();

    loop {
        tokio::select! {
            _notified = notify.notified() => {
                // Someone has updated the clipboard. Send it to everyone else.
                let new_clip = CLIPBOARD.read().await;
                // We're holding a read lock while we write the bytes into
                // the buffer. Probably not a big deal?
                writer.write_all(new_clip.as_bytes_with_nul()).await?;
                debug!("Sent {new_clip:?} to {remote_addr}");
                drop(new_clip);
                writer.flush().await?;
            },
            _readable = reader.get_ref().readable() => {
                let mut incoming_bytes = Vec::new();

                // TODO handle incomplete message?
                let bytes_read = reader.read_until(0, &mut incoming_bytes).await?;
                if bytes_read == 0 {
                    // EOF
                    return Ok(())
                }

                let new_clip = EncodedClipboard::from_bytes(incoming_bytes);
                debug!("Received {new_clip:?} from {remote_addr}");

                let decoded = match new_clip.decode() {
                    Ok(d) => d,
                    Err(e) => {
                        error!("Couldn't decode incoming clipboard. What happened? {e}");
                        continue;
                    }
                };
                clip_ctx.set_text(decoded)?;
                *CLIPBOARD.write().await = new_clip;

                // We're not a waiter, so we won't get woken up by our own update later
                notify.notify_waiters();
            }
        }
    }
}
