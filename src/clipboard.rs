use crate::{ClipboardChange, CLIPBOARD};
use std::time::Duration;
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

pub struct Clipboard<B> {
    board: B,
    listener: Listener,
    poll_interval: Duration,
}

impl<B: Board> Clipboard<B> {
    pub fn new(poll_interval: Duration) -> anyhow::Result<Self> {
        let board = B::new()?;
        let listener = Listener::new()?;

        Ok(Self {
            board,
            listener,
            poll_interval,
        })
    }

    pub async fn listen_for_change(&mut self) -> anyhow::Result<ClipboardChange> {
        loop {
            let from_polling =
                match tokio::time::timeout(self.poll_interval, self.listener.change()).await {
                    Err(_) => true,
                    Ok(x) => {
                        x?;
                        false
                    }
                };
            let Some(text_contents) = self.board.get_text()? else {
                continue;
            };

            let change = ClipboardChange::Text(text_contents);
            if Some(&change) == CLIPBOARD.read().await.as_ref() {
                if !from_polling {
                    debug!("Saw a clipboard change event but the text actually hadn't changed");
                }
                continue;
            }

            if from_polling {
                // TODO this error message needs to be silenceable (fire a few times max maybe)
                warn!("There was a clipboard change but I didn't see a clipboard change event for it: {change}. Consider reducing the poll interval if this is expected, or submit a bug report otherwise!");
            } else {
                debug!("Local clipboard change: {change}");
            }
            return Ok(change);
        }
    }
}

pub trait Board: Sized + Send {
    fn new() -> anyhow::Result<Self>;
    /// Try to set the clipboard text. Should not panic
    fn set_text(&mut self, text: &str);
    /// Returns [`None`] if the clipboard was empty/unavailable
    /// Should only return an error on unrecoverable failures.
    fn get_text(&mut self) -> anyhow::Result<Option<String>>;
}

impl Board for arboard::Clipboard {
    fn new() -> anyhow::Result<Self> {
        Ok(arboard::Clipboard::new()?)
    }
    fn set_text(&mut self, text: &str) {
        if let Err(e) = self.set_text(text) {
            error!("Couldn't set clipboard text: {e}");
        }
    }
    fn get_text(&mut self) -> anyhow::Result<Option<String>> {
        use arboard::Error;
        match self.get_text() {
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
            // arboard::Error is marked as non_exhaustive so we need a catch-all;
            // just fall over.
            Err(e) => Err(e.into()),
        }
    }
}
