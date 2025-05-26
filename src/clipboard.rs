use tracing::*;

pub trait Clipboard: Sized + Send {
    fn new() -> anyhow::Result<Self>;
    /// Try to set the clipboard text. Should not panic
    fn set_text(&mut self, text: &str);
    /// Returns [`None`] if the clipboard was empty/unavailable
    /// Should only return an error on unrecoverable failures.
    fn get_text(&mut self) -> anyhow::Result<Option<String>>;
}

impl Clipboard for arboard::Clipboard {
    fn new() -> anyhow::Result<Self> {
        Ok(Self::new()?)
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
