use crate::IntoAnyhow;
use arboard::ImageData;
use enum_map::{Enum, EnumMap};
use rustc_hash::FxHasher;
use std::{
    fmt,
    hash::{Hash, Hasher},
    io,
    time::Duration,
};
use tokio::sync::RwLock;
use tracing::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
type Listener = linux::Listener;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
type Listener = macos::Listener;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
type Listener = windows::Listener;

pub static LATEST_CHANGE: RwLock<Option<ClipboardChange>> = RwLock::const_new(None);

static CLIPBOARD_HASHES: RwLock<EnumMap<ClipboardKind, Option<u64>>> =
    RwLock::const_new(EnumMap::from_array([None; <ClipboardKind as Enum>::LENGTH]));

#[derive(Debug)]
pub enum ClipboardChange {
    Text(String),
    Image(arboard::ImageData<'static>),
}

impl ClipboardChange {
    pub fn len(&self) -> usize {
        match self {
            Self::Text(x) => x.len(),
            Self::Image(x) => x.width * x.height + 2 * std::mem::size_of::<usize>(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn kind(&self) -> ClipboardKind {
        match self {
            ClipboardChange::Text(_) => ClipboardKind::Text,
            ClipboardChange::Image(_) => ClipboardKind::Image,
        }
    }

    pub fn write_all(&self, mut writer: impl io::Write) -> io::Result<()> {
        match self {
            ClipboardChange::Text(x) => {
                writer.write_all(x.as_bytes())?;
            }
            ClipboardChange::Image(x) => {
                writer.write_all(x.bytes.as_ref())?;
                writer.write_all(u64::try_from(x.width).unwrap().to_le_bytes().as_slice())?;
                writer.write_all(u64::try_from(x.height).unwrap().to_le_bytes().as_slice())?;
            }
        }
        Ok(())
    }
}

impl PartialEq for ClipboardChange {
    fn eq(&self, other: &Self) -> bool {
        use ClipboardChange::*;
        match (self, other) {
            (Text(a), Text(b)) => a == b,
            (Image(a), Image(b)) => {
                a.width == b.width && a.height == b.height && a.bytes == b.bytes
            }
            _ => false,
        }
    }
}

impl fmt::Display for ClipboardChange {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Text(s) if s.len() < 100 => write!(f, "Text: {s}"),
            Self::Text(s) => write!(f, "Text (omitted, {} bytes long)", s.len()),
            Self::Image(s) => write!(f, "{}x{} image", s.width, s.height),
        }
    }
}

impl Hash for ClipboardChange {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self {
            ClipboardChange::Text(x) => x.hash(h),
            ClipboardChange::Image(x) => {
                x.width.hash(h);
                x.height.hash(h);
                x.bytes.hash(h);
            }
        }
    }
}

#[derive(Debug, Enum)]
pub enum ClipboardKind {
    Text,
    Image,
}

pub struct Clipboard<B> {
    board: B,
    listener: Option<Listener>,
    poll_interval: Duration,
}

impl<B: Board> Clipboard<B> {
    pub fn new(poll_interval: Duration) -> anyhow::Result<Self> {
        let board = B::new()?;
        let listener = Listener::new();

        if let Err(e) = listener.as_ref() {
            warn!("\
Failed to start clipboard listener: {e}. This might not be a surprise to you (non-X11 linux?) but if it is, please raise a bug report! \
In the meantime, you're stuck witih polling.");
        }

        Ok(Self {
            board,
            listener: listener.ok(),
            poll_interval,
        })
    }

    pub async fn listen_for_change(&mut self) -> anyhow::Result<ClipboardChange> {
        loop {
            let from_polling = match self.listener.as_ref() {
                Some(listener) => {
                    let res = tokio::time::timeout(self.poll_interval, listener.change()).await;
                    match res {
                        Ok(Ok(())) => false,
                        Ok(Err(e)) => {
                            error!("Failed to listen for clipboard updates: {e}");
                            continue;
                        }
                        Err(_elapsed) => true,
                    }
                }
                None => {
                    tokio::time::sleep(self.poll_interval).await;
                    false
                }
            };

            let hashes = *CLIPBOARD_HASHES.read().await;
            let text = self
                .board
                .get_text()?
                .map(ClipboardChange::Text)
                .filter(|x| hashes[ClipboardKind::Text] != Some(hash(x)));
            let image = self
                .board
                .get_image()?
                .map(ClipboardChange::Image)
                .filter(|x| hashes[ClipboardKind::Image] != Some(hash(x)));

            let Some(change) = text.or(image) else {
                if !from_polling {
                    debug!("Saw a clipboard change event but nothing had actually changed");
                }
                continue;
            };

            if from_polling {
                // TODO this error message needs to be silenceable (fire a few times max maybe)
                warn!("There was a clipboard change but I didn't see a clipboard change event for it: {change}. Consider reducing the poll interval if this is expected, or submit a bug report otherwise!");
            } else {
                debug!("Local clipboard change: {change}");
            }
            store_hash(Some(&change)).await;
            return Ok(change);
        }
    }
}

pub trait Board: Sized + Send {
    fn new() -> anyhow::Result<Self>;
    /// Try to set the clipboard text. Should not panic
    fn set_text(&mut self, text: &str);
    fn set_image<'a>(&mut self, image: ImageData<'a>);
    /// Returns [`None`] if the clipboard was empty/unavailable
    /// Should only return an error on unrecoverable failures.
    fn get_text(&mut self) -> anyhow::Result<Option<String>>;
    fn get_image(&mut self) -> anyhow::Result<Option<ImageData<'static>>>;
}

impl Board for arboard::Clipboard {
    fn new() -> anyhow::Result<Self> {
        arboard::Clipboard::new().into_anyhow("Failed to instantiate clipboard")
    }
    fn set_text(&mut self, text: &str) {
        if let Err(e) = self.set_text(text) {
            error!("Couldn't set clipboard text: {e}");
        }
    }
    fn set_image<'a>(&mut self, image: ImageData<'a>) {
        if let Err(e) = self.set_image(image) {
            error!("Couldn't set clipboard image: {e}");
        }
    }
    fn get_text(&mut self) -> anyhow::Result<Option<String>> {
        match self.get_text() {
            // Don't broadcast clipboard reset!
            Ok(s) if s.is_empty() => Ok(None),
            Ok(s) => Ok(Some(s)),
            Err(e) => {
                if let Some(e) = handle_err(e) {
                    Err(e.into())
                } else {
                    Ok(None)
                }
            }
        }
    }
    fn get_image(&mut self) -> anyhow::Result<Option<ImageData<'static>>> {
        match self.get_image() {
            Ok(x) => Ok(Some(x)),
            Err(e) => {
                if let Some(e) = handle_err(e) {
                    Err(e.into())
                } else {
                    Ok(None)
                }
            }
        }
    }
}

pub async fn store_hash(change: Option<&ClipboardChange>) {
    let Some(change) = change else {
        return;
    };

    let mut lock = CLIPBOARD_HASHES.write().await;
    lock[change.kind()] = Some(hash(change));
}

fn hash<T: Hash>(x: T) -> u64 {
    let mut hasher = FxHasher::default();
    x.hash(&mut hasher);
    hasher.finish()
}

fn handle_err(error: arboard::Error) -> Option<arboard::Error> {
    use arboard::Error;
    match error {
        // Retry
        Error::ContentNotAvailable | Error::ClipboardOccupied => None,
        // For text, this means non-utf8 AFAICT.
        Error::ConversionFailure => {
            warn!("Couldn't convert clipboard contents to desired type");
            None
        }
        Error::Unknown { description } => {
            warn!("Error reading clipboard: {description}");
            // Retry? From a quick look at the library's source, these errors seem
            // transient
            None
        }
        // No point retrying on this
        e @ Error::ClipboardNotSupported => Some(e),
        // arboard::Error is marked as non_exhaustive so we need a catch-all;
        // just fall over.
        e => Some(e),
    }
}
