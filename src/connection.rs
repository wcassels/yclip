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
    noise: Noise,
    read_buf: Vec<u8>,
    cobs_buf: Vec<u8>,
}

impl<T: AsyncRead + AsyncWrite> Connection<T> {
    pub fn new(transport: T, peer_addr: impl Display, noise: Noise) -> Self {
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
        let to_encode = self.noise.encode_message(clipboard)?;
        self.cobs_buf
            .resize(cobs::max_encoding_length(to_encode.len()), 0);
        let encoded_len = cobs::encode(to_encode, &mut self.cobs_buf);
        self.cobs_buf.truncate(encoded_len);
        self.cobs_buf.push(0);
        self.writer.write_all(self.cobs_buf.as_slice()).await?;
        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<ReadResult> {
        // This future is cancel-safe, and awaiting it is the first thing we do -
        // which means that Connection::read is cancel-safe too. Think before moving it
        // or adding more async components
        match self.reader.read_until(0, &mut self.read_buf).await {
            Ok(0) => return Ok(ReadResult::Eof),
            Ok(bytes_read) if self.read_buf.last().copied() != Some(0) => {
                debug!(
                    "Received incomplete message from {}. This read: {bytes_read}, total: {} bytes",
                    self.peer_addr,
                    self.read_buf.len()
                );
                return Ok(ReadResult::Incomplete);
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                return Ok(ReadResult::Eof)
            }
            Err(e) => return Err(e.into()),
        }

        self.read_buf.pop();
        let len = cobs::decode_in_place(&mut self.read_buf)?;
        self.read_buf.truncate(len);
        let to_decode = self.noise.decode_message(&self.read_buf)?;
        let result = String::from_utf8(to_decode.to_vec())?;
        self.read_buf.clear();

        Ok(ReadResult::Done(result))
    }
    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }
}

#[derive(Debug)]
pub enum ReadResult {
    Eof,
    Incomplete,
    Done(String),
}
