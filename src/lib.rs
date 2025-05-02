use arboard::Clipboard;
use std::{
    ffi::CString,
    net::{SocketAddr, SocketAddrV4},
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Notify, RwLock},
};
use tracing::*;

#[derive(Debug, PartialEq, Eq)]
struct EncodedClipboard(CString);

impl EncodedClipboard {
    pub fn decode(&self) -> String {
        let utf8 = postcard_cobs::decode_vec(self.0.as_bytes()).unwrap();
        String::from_utf8(utf8).unwrap()
    }

    pub fn encode(clipboard: &str) -> Self {
        let mut bytes = postcard_cobs::encode_vec(clipboard.as_bytes());
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

static CLIPBOARD: LazyLock<RwLock<EncodedClipboard>> = LazyLock::new(|| {
    let clipboard = Clipboard::new().unwrap().get_text().unwrap();
    RwLock::const_new(EncodedClipboard::encode(clipboard.as_str()))
});

pub async fn run_satellite(addr: SocketAddrV4, refresh_rate: Duration) -> anyhow::Result<()> {
    let stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;
    info!("Connected to clipboard on {addr}");
    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_rate);
    watch_remote(stream, notify).await?; // Blocks
    info!("Host disconnected");

    Ok(())
}

pub async fn run_host(port: u16, refresh_interval: Duration) -> anyhow::Result<()> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new([0, 0, 0, 0].into(), port)).await?;

    info!("Listening for connections on port {port}");

    // What do we need to do?
    // 1. Monitor local clipboard & broadcast to all non-self hosts
    // 2. Listen for incoming clipboard updates & broadcast to all other hosts

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify), refresh_interval);

    loop {
        let (stream, _addr) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let client_addr = stream.peer_addr()?;
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

    loop {
        tokio::time::sleep(refresh_rate).await;
        let new_clip = ctx.get_text().unwrap();
        let encoded = EncodedClipboard::encode(&new_clip);
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

                let decoded = new_clip.decode();
                clip_ctx.set_text(decoded).unwrap();
                *CLIPBOARD.write().await = new_clip;

                // We're not a waiter, so we won't get woken up by our own update later
                notify.notify_waiters();
            }
        }
    }
}
