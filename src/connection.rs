use crate::{secure::Noise, ClipboardChange, IntoAnyhow};
use arboard::ImageData;
use std::fmt::Display;
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, ReadHalf, WriteHalf,
};
use tracing::*;

const MIN_COMPRESSION_LEN: usize = 1024;
const CHUNK_SIZE: usize = 63 * 1024;
const MAX_CHUNKS: usize = 1000;
const METADATA_LEN: usize = 1 + 1 + 2;
const CHUNK_HEADER_LEN: usize = 2 + 2;

const _: () = assert!(MIN_COMPRESSION_LEN < CHUNK_SIZE);

pub struct Connection<T: AsyncRead + AsyncWrite> {
    reader: BufReader<ReadHalf<T>>,
    writer: BufWriter<WriteHalf<T>>,
    peer_addr: String,
    noise: Noise,
    read_buf: Vec<u8>,
    finished: Vec<u8>,
    compressed: Vec<u8>,
    current_meta: Option<Metadata>,
}

impl<T: AsyncRead + AsyncWrite> Connection<T> {
    pub fn new(transport: T, peer_addr: impl Display, noise: Noise) -> Self {
        let (reader, writer) = tokio::io::split(transport);
        Self {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            peer_addr: peer_addr.to_string(),
            noise,
            read_buf: Vec::new(),
            finished: Vec::new(),
            compressed: Vec::new(),
            current_meta: None,
        }
    }

    pub async fn send(
        &mut self,
        update: &ClipboardChange,
        peer: impl Display,
    ) -> anyhow::Result<()> {
        match self.send_inner(update).await {
            Ok(()) => {
                debug!("Sent {update} to {peer}");
                Ok(())
            }
            Err(e) if e.recoverable() => {
                error!("Failed to send {update} to {peer}: {e}");
                Ok(())
            }
            x @ Err(_) => x.into_anyhow(format_args!("failed to send {update} to {peer}")),
        }
    }

    async fn send_inner(&mut self, update: &ClipboardChange) -> Result<(), SendError> {
        if update.is_empty() {
            return Err(SendError::ClipboardEmpty);
        }

        self.compressed.clear();
        let compress = update.len() > MIN_COMPRESSION_LEN;

        if compress {
            let mut encoder = zstd::Encoder::new(&mut self.compressed, 0)?;
            update.write_all(&mut encoder).map_err(SendError::Zstd)?;
            encoder.finish()?;
        } else {
            update.write_all(&mut self.compressed)?;
        }

        let n_chunks = 1 + self.compressed.len() / CHUNK_SIZE;
        if n_chunks > MAX_CHUNKS {
            return Err(SendError::ClipboardTooBig {
                bytes: self.compressed.len(),
            });
        }

        let n_chunks = u16::try_from(n_chunks).unwrap();

        // kind | compress | n_chunks | (idx | len | data) repeat

        self.writer
            .write_u8(match &update {
                ClipboardChange::Text(_) => 0,
                ClipboardChange::Image(_) => 1,
            })
            .await?;
        self.writer.write_u8(if compress { 1 } else { 0 }).await?;
        self.writer.write_u16_le(n_chunks).await?;

        for (idx, chunk) in (0..).zip(self.compressed.chunks(CHUNK_SIZE)) {
            let encoded = self.noise.encode_message(chunk)?;
            self.writer.write_u16_le(idx).await?;
            let len = u16::try_from(encoded.len()).unwrap();
            self.writer.write_u16_le(len).await?;
            self.writer.write_all(encoded).await?;
        }

        self.writer.flush().await?;

        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<Option<ClipboardChange>> {
        match self.read_inner().await {
            Ok((bytes, meta)) if meta.is_text() => {
                Ok(Some(ClipboardChange::Text(String::from_utf8(bytes)?)))
            }
            Ok((mut bytes, _meta)) => {
                let len = bytes.len();
                anyhow::ensure!(
                    len >= 2 * std::mem::size_of::<u64>(),
                    "I didn't even receive enough bytes to read a width and a height?"
                );
                let width_bytes = bytes[len - 2 * 8..len - 8].try_into()?;
                let height_bytes = bytes[len - 8..].try_into()?;
                let width = usize::try_from(u64::from_le_bytes(width_bytes))?;
                let height = usize::try_from(u64::from_le_bytes(height_bytes))?;
                bytes.truncate(len - 2 * 8);
                Ok(Some(ClipboardChange::Image(ImageData {
                    width,
                    height,
                    bytes: std::borrow::Cow::Owned(bytes),
                })))
            }
            Err(ReadError::Eof) => Ok(None),
            // It feels like it would be nice to recover from some of these. But is that
            // worth the effort? (I don't think so)
            Err(e) => anyhow::bail!("failed to read incoming message: {e}"),
        }
    }

    // Have to be quite a bit more careful to make this cancel-safe
    // (basically have to chain cancel-safe futures, and persist
    // all state in case any of them are cancelled)
    async fn read_inner(&mut self) -> Result<(Vec<u8>, Metadata), ReadError> {
        let meta = match &mut self.current_meta {
            Some(m) => m,
            None => {
                let mut meta = Metadata::default();
                match self.reader.read_exact(&mut meta.meta_bytes).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Err(ReadError::Eof)
                    }
                    Err(e) => return Err(e.into()),
                }
                debug!(
                    "New incoming message: is_text={}, compress={}, n_chunks={}",
                    meta.is_text(),
                    meta.compress(),
                    meta.n_chunks()
                );
                self.current_meta.insert(meta)
            }
        };

        let compress = meta.compress();
        let n_chunks = meta.n_chunks();

        loop {
            let chunk_header = match &mut meta.current_chunk_header {
                Some(h) => h,
                None => {
                    let mut header = [0; CHUNK_HEADER_LEN];
                    self.reader.read_exact(&mut header).await?;
                    debug!(
                        "Chunk {} has len {}",
                        Metadata::chunk_idx(header),
                        Metadata::chunk_len(header)
                    );
                    meta.current_chunk_header.insert(header)
                }
            };

            let chunk_len = Metadata::chunk_len(*chunk_header);
            let chunk_idx = Metadata::chunk_idx(*chunk_header);

            self.read_buf.resize(chunk_len, 0);
            self.reader.read_exact(&mut self.read_buf).await?;
            let decoded = self.noise.decode_message(&self.read_buf)?;

            if !compress {
                if n_chunks != 1 {
                    return Err(ReadError::Adhoc(anyhow::anyhow!(
                        "Saw a message >63KB that wasn't compressed!"
                    )));
                }
                let meta = self.current_meta.take().unwrap();
                return Ok((decoded.to_vec(), meta));
            }

            // TODO: Less copying
            self.finished.extend_from_slice(decoded);

            if chunk_idx == n_chunks - 1 {
                // Done!
                let mut decompressed = Vec::new();
                zstd::stream::copy_decode(self.finished.as_slice(), &mut decompressed)
                    .map_err(ReadError::Zstd)?;
                self.finished.clear();
                let meta = self.current_meta.take().unwrap();
                return Ok((decompressed, meta));
            }

            meta.current_chunk_header.take();
            debug!("Finished chunk {chunk_idx}");
        }
    }

    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("IO error {0}")]
    Io(#[from] std::io::Error),
    #[error("encode error {0}")]
    Noise(#[from] snow::Error),
    #[error("compression error {0}")]
    Zstd(std::io::Error),
    #[error("shouldn't be trying to send an empty clipboard")]
    ClipboardEmpty,
    #[error(
        "clipboard too big ({}MB post-compression - the limit is 63MB)",
        bytes / (1024 * 1024)
    )]
    ClipboardTooBig { bytes: usize },
}

impl SendError {
    fn recoverable(&self) -> bool {
        match self {
            Self::ClipboardEmpty => true,
            Self::ClipboardTooBig { .. } => true,
            Self::Zstd(_) => true,
            // If we return either of the below, there's a chance we've already
            // written an incomplete message to the socket.
            Self::Io(_) => false,
            Self::Noise(_) => false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("IO error {0}")]
    Io(#[from] std::io::Error),
    #[error("EOF")]
    Eof,
    #[error("decode error {0}")]
    Noise(#[from] snow::Error),
    #[error("decompression error {0}")]
    Zstd(std::io::Error),
    #[error("clipboard wasn't utf8")]
    NonUt8(#[from] std::string::FromUtf8Error),
    #[error("logic error {0}")]
    Adhoc(#[from] anyhow::Error),
}

#[derive(Clone, Copy, Default)]
struct Metadata {
    meta_bytes: [u8; METADATA_LEN],
    current_chunk_header: Option<[u8; CHUNK_HEADER_LEN]>,
}

impl Metadata {
    pub fn compress(&self) -> bool {
        self.meta_bytes[1] == 1
    }

    pub fn n_chunks(&self) -> u16 {
        u16::from_le_bytes([self.meta_bytes[2], self.meta_bytes[3]])
    }

    pub fn chunk_len(chunk_header: [u8; CHUNK_HEADER_LEN]) -> usize {
        let n = u16::from_le_bytes([chunk_header[2], chunk_header[3]]);
        usize::from(n)
    }

    pub fn chunk_idx(chunk_header: [u8; CHUNK_HEADER_LEN]) -> u16 {
        u16::from_le_bytes([chunk_header[0], chunk_header[1]])
    }

    pub fn is_text(&self) -> bool {
        self.meta_bytes[0] == 0
    }
}
