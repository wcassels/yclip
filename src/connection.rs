use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
};
use tracing::*;

pub struct Connection<T: Transport> {
    reader: T::Reader,
    writer: T::Writer,
    peer_addr: String,
    noise: Option<crate::secure::Noise>,
    read_buf: Vec<u8>,
    cobs_buf: Vec<u8>,
}

pub trait Transport {
    type Reader: AsyncBufReadExt + Unpin;
    type Writer: AsyncWriteExt + Unpin;
}

impl Transport for TcpStream {
    type Reader = BufReader<OwnedReadHalf>;
    type Writer = OwnedWriteHalf;
}

impl<T: Transport> Connection<T> {
    /// Assumes `clipboard` is non-empty
    pub async fn send(&mut self, clipboard: &str) -> anyhow::Result<()> {
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

impl Connection<TcpStream> {
    pub fn tcp(stream: TcpStream, peer_addr: String, noise: Option<crate::secure::Noise>) -> Self {
        let (read, writer) = stream.into_split();
        Self {
            reader: tokio::io::BufReader::new(read),
            writer,
            peer_addr,
            noise,
            read_buf: Vec::new(),
            cobs_buf: Vec::new(),
        }
    }
}

pub enum ReadResult {
    Eof,
    Incomplete,
    Done(String),
}
