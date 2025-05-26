use crate::secure::Noise;
use std::fmt::Display;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tracing::*;

pub struct Connection<T> {
    reader: BufReader<ReadHalf<T>>,
    writer: WriteHalf<T>,
    peer_addr: String,
    noise: Option<Noise>,
    read_buf: Vec<u8>,
    cobs_buf: Vec<u8>,
}

impl<T: AsyncRead + AsyncWrite> Connection<T> {
    pub fn new(transport: T, peer_addr: impl Display, noise: Option<Noise>) -> Self {
        let (reader, writer) = tokio::io::split(transport);
        Self {
            reader: BufReader::new(reader),
            writer,
            peer_addr: peer_addr.to_string(),
            noise,
            read_buf: Vec::new(),
            cobs_buf: Vec::new(),
        }
    }
    /// Assumes `clipboard` is non-empty
    pub async fn send(&mut self, clipboard: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !clipboard.is_empty(),
            "Logic bug! Shouldn't be trying to send an empty clipboard"
        );
        let to_encode = if let Some(n) = self.noise.as_mut() {
            n.encode_message(clipboard)?
        } else {
            clipboard.as_bytes()
        };
        self.cobs_buf
            .resize(cobs::max_encoding_length(to_encode.len()), 0);
        let encoded_len = cobs::encode(to_encode, &mut self.cobs_buf);
        self.cobs_buf.truncate(encoded_len);
        self.cobs_buf.push(0);
        self.writer.write_all(self.cobs_buf.as_slice()).await?;
        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<ReadResult> {
        // This await is cancel-safe which means the whole method is... don't add any more awaits without
        // thinking about it
        let bytes_read = self.reader.read_until(0, &mut self.read_buf).await?;
        if bytes_read == 0 {
            return Ok(ReadResult::Eof);
        } else if self.read_buf.last().copied() != Some(0) {
            debug!(%self.peer_addr, "Mid-way through receiving incomplete message. This read: {bytes_read}, total: {} bytes", self.read_buf.len());
            return Ok(ReadResult::Incomplete);
        }

        self.read_buf.pop();
        let len = cobs::decode_in_place(&mut self.read_buf)?;
        self.read_buf.truncate(len);
        let to_decode = if let Some(n) = self.noise.as_mut() {
            n.decode_message(&self.read_buf)?
        } else {
            &self.read_buf
        };
        let result = String::from_utf8(to_decode.to_vec())?;
        self.read_buf.clear();

        Ok(ReadResult::Done(result))
    }
    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }
}

pub enum ReadResult {
    Eof,
    Incomplete,
    Done(String),
}
