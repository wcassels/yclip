use clipboard::{ClipboardContext, ClipboardProvider};
use std::{
    ffi::CString,
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Notify, RwLock},
};

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

    pub fn as_bytes(&self) -> &[u8] {
        self.0.to_bytes()
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(CString::from_vec_with_nul(bytes).unwrap())
    }
}

static CLIPBOARD: RwLock<Option<EncodedClipboard>> = RwLock::const_new(None);

pub async fn run_satellite(addr: SocketAddrV4) -> anyhow::Result<()> {
    let stream = TcpStream::connect(addr).await?;
    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify));
    watch_remote(stream, notify).await?; // Blocks

    Ok(())
}

pub async fn run_host(port: u16) -> anyhow::Result<()> {
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new([0, 0, 0, 0].into(), port)).await?;

    // What do we need to do?
    // 1. Monitor local clipboard & broadcast to all non-self hosts
    // 2. Listen for incoming clipboard updates & broadcast to all other hosts

    let notify = Arc::new(Notify::const_new());
    spawn_local_watcher(Arc::clone(&notify));

    loop {
        let (stream, _addr) = listener.accept().await?;
        let notify = Arc::clone(&notify);
        spawn_remote_watcher(stream, Arc::clone(&notify));
    }
}

fn spawn_remote_watcher(stream: TcpStream, notify: Arc<Notify>) {
    tokio::task::spawn(async {
        if let Err(e) = watch_remote(stream, notify).await {
            // log!
        }
    });
}

fn spawn_local_watcher(notify: Arc<Notify>) {
    tokio::task::spawn(async {
        if let Err(e) = watch_local(notify).await {
            // log!
        }
    });
}

async fn watch_local(notify: Arc<Notify>) -> anyhow::Result<()> {
    let mut ctx = ClipboardContext::new().unwrap();
    let mut current_clip = ctx.get_contents().unwrap();

    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let new_clip = ctx.get_contents().unwrap();
        if new_clip != current_clip {
            let encoded = EncodedClipboard::encode(&new_clip);
            *CLIPBOARD.write().await = Some(encoded);
            notify.notify_waiters();
            current_clip = new_clip;
        }
    }
}

async fn watch_remote(mut stream: TcpStream, notify: Arc<Notify>) -> anyhow::Result<()> {
    let (read, write) = stream.split();
    let mut writer = tokio::io::BufWriter::new(write);
    let mut reader = tokio::io::BufReader::new(read);
    let mut clip_ctx = ClipboardContext::new().unwrap();

    tokio::select! {
        _notified = notify.notified() => {
            // Someone has updated the clipboard. Send it to everyone else.
            let lock = CLIPBOARD.read().await;
            let new_clip = lock
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Invariant breached: notified before clipboard was initialised?"))?;
            // We're holding a read lock while we write the bytes into
            // the buffer. Probably not a big deal?
            writer.write_all(new_clip.as_bytes()).await?;
            drop(lock);
            writer.flush().await?;
        },
        _readable = reader.get_ref().readable() => {
            let mut incoming_bytes = Vec::new();
            reader.read_until(0, &mut incoming_bytes).await?;
            let new_clip = EncodedClipboard::from_bytes(incoming_bytes);

            let decoded = new_clip.decode();
            clip_ctx.set_contents(decoded).unwrap();
            *CLIPBOARD.write().await = Some(new_clip);

            // We're not a waiter, so we won't get woken up by our own update later
            notify.notify_waiters();
        }
    }

    Ok(())
}
