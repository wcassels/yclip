use crate::{secure::Noise, ClipboardChange};
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

pub struct Connection<T: AsyncRead + AsyncWrite> {
    reader: BufReader<ReadHalf<T>>,
    writer: BufWriter<WriteHalf<T>>,
    peer_addr: String,
    noise: Noise,
    read_buf: Vec<u8>,
    finished: Vec<u8>,
    compress_buf: Vec<u8>,
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
            compress_buf: Vec::new(),
            current_meta: None,
        }
    }

    pub async fn send(&mut self, update: &ClipboardChange) -> anyhow::Result<()> {
        let bytes = match &update {
            ClipboardChange::Text(x) => x.as_bytes(),
            ClipboardChange::Image(x) => x.as_slice(),
        };
        anyhow::ensure!(
            !bytes.is_empty(),
            "Logic bug! Shouldn't be trying to send an empty clipboard"
        );
        let compress = bytes.len() > MIN_COMPRESSION_LEN;
        let to_encode = if compress {
            zstd::stream::copy_encode(bytes, &mut self.compress_buf, 0)?;
            &self.compress_buf
        } else {
            bytes
        };

        let n_chunks = 1 + to_encode.len() / CHUNK_SIZE;
        anyhow::ensure!(
            n_chunks < MAX_CHUNKS,
            "Refusing to send large clipboard contents (>63MB, even after compression)"
        );
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

        for (idx, chunk) in (0..).zip(to_encode.chunks(CHUNK_SIZE)) {
            let encoded = self.noise.encode_message(chunk)?;
            self.writer.write_u16_le(idx).await?;
            let len = u16::try_from(encoded.len()).unwrap();
            self.writer.write_u16_le(len).await?;
            self.writer.write_all(encoded).await?;
        }

        self.writer.flush().await?;

        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<ReadResult> {
        match self.read_inner().await {
            Ok(x) => Ok(ReadResult::Done(x)),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Ok(ReadResult::Eof)
            }
            Err(e) => Err(e.into()),
        }
    }

    // Have to be quite a bit more careful to make this cancel-safe
    // (basically have to chain cancel-safe futures, and persist
    // all state in case any of them are cancelled)
    async fn read_inner(&mut self) -> Result<String, Error> {
        let meta = match &mut self.current_meta {
            Some(m) => m,
            None => {
                let mut meta = Metadata::default();
                self.reader.read_exact(&mut meta.meta_bytes).await?;
                self.current_meta.insert(meta)
            }
        };
        if !meta.is_text() {
            return Err(Error::Adhoc(anyhow::anyhow!(
                "Image syncing isn't supported yet!"
            )));
        }
        let compress = meta.compress();
        let n_chunks = meta.n_chunks();

        loop {
            let chunk_header = match &mut meta.current_chunk_header {
                Some(h) => h,
                None => {
                    let mut header = [0; CHUNK_HEADER_LEN];
                    self.reader.read_exact(&mut header).await?;
                    meta.current_chunk_header.insert(header)
                }
            };

            let chunk_len = Metadata::chunk_len(*chunk_header);
            let chunk_idx = Metadata::chunk_idx(*chunk_header);

            self.read_buf.resize(chunk_len, 0);
            self.reader.read_exact(&mut self.read_buf).await?;
            let decoded = self.noise.decode_message(&self.read_buf)?;

            if compress {
                zstd::stream::copy_decode(decoded, &mut self.finished)?;
            } else {
                if n_chunks != 1 {
                    return Err(Error::Adhoc(anyhow::anyhow!(
                        "Saw a message >63KB that wasn't compressed!"
                    )));
                }
                let clipboard = std::str::from_utf8(decoded)?.to_string();
                self.current_meta.take();
                return Ok(clipboard);
            }

            if chunk_idx == n_chunks - 1 {
                // Done!
                let clipboard = std::str::from_utf8(&self.finished)?.to_string();
                self.current_meta.take();
                return Ok(clipboard);
            }

            meta.current_chunk_header.take();
        }
    }

    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Decode error: {0}")]
    Noise(#[from] snow::Error),
    #[error("Clipboard wasn't utf8: {0:?}")]
    NonUt8(#[from] std::str::Utf8Error),
    #[error("Logic error: {0}")]
    Adhoc(#[from] anyhow::Error),
}

#[derive(Debug)]
pub enum ReadResult {
    Eof,
    Incomplete,
    Done(String),
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
