use rand::Rng;
use snow::{params::NoiseParams, Builder, HandshakeState, TransportState};
use std::{fmt::Display, io, sync::LazyLock};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tracing::*;

// In NN0, the psk immediately gets mixed into the chaining key.
// -> psk, e
// <- e, ee
static PARAMS: LazyLock<NoiseParams> =
    LazyLock::new(|| "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s".parse().unwrap());

pub struct Noise {
    transport: TransportState,
    buf: Vec<u8>,
}

#[derive(thiserror::Error, Debug)]
pub enum HandshakeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Noise error: {0}")]
    Noise(#[from] snow::Error),
}

impl HandshakeError {
    pub fn is_likely_password_mismatch(&self) -> bool {
        match self {
            Self::Noise(_) => true,
            Self::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => true,
            Self::Io(_) => false,
        }
    }
}

impl Noise {
    pub async fn host<I>(
        stream: &mut I,
        client_addr: impl Display,
        secret: &Secret,
    ) -> Result<Self, HandshakeError>
    where
        I: AsyncRead + AsyncWrite + Unpin,
    {
        debug!("New incoming connection from {client_addr}, starting secure handshake...");
        let mut buf = vec![0; 65536];

        Self::handshake_send(stream, secret.salt()).await?;

        let mut handshake = Self::init_handshake(secret.hash(), false)?;

        handshake.read_message(&Self::handshake_recv(stream).await?, &mut buf)?;
        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;
        debug!("Handshake with {client_addr} complete!");

        Ok(Self {
            transport: handshake.into_transport_mode()?,
            buf,
        })
    }

    pub async fn satellite<I>(stream: &mut I, secret: &str) -> Result<Self, HandshakeError>
    where
        I: AsyncRead + AsyncWrite + Unpin,
    {
        let mut buf = vec![0; 65536];
        debug!("Established connection, starting secure handshake...");
        let salt: [u8; 32] = Self::handshake_recv(stream).await?.try_into().unwrap();
        let secret = Secret::new(secret, Some(salt));
        let mut handshake = Self::init_handshake(secret.hash(), true)?;

        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;
        handshake.read_message(&Self::handshake_recv(stream).await?, &mut buf)?;
        debug!("Handshake complete!");

        Ok(Self {
            transport: handshake.into_transport_mode()?,
            buf,
        })
    }

    pub fn encode_message(&mut self, msg: &str) -> anyhow::Result<&[u8]> {
        let len = self
            .transport
            .write_message(msg.as_bytes(), &mut self.buf)?;
        Ok(&self.buf[..len])
    }

    pub fn decode_message(&mut self, msg: &[u8]) -> anyhow::Result<&[u8]> {
        let len = self.transport.read_message(msg, &mut self.buf)?;
        Ok(&self.buf[..len])
    }

    fn init_handshake(hash: &[u8; 32], initiator: bool) -> Result<HandshakeState, HandshakeError> {
        let builder = Builder::new(PARAMS.clone()).psk(0, hash);
        if initiator {
            Ok(builder.build_initiator()?)
        } else {
            Ok(builder.build_responder()?)
        }
    }

    async fn handshake_recv<I: AsyncRead + Unpin>(stream: &mut I) -> io::Result<Vec<u8>> {
        let mut msg_len_buf = [0_u8; 2];
        stream.read_exact(&mut msg_len_buf).await?;
        let msg_len = usize::from(u16::from_be_bytes(msg_len_buf));
        let mut msg = vec![0_u8; msg_len];
        stream.read_exact(&mut msg[..]).await?;
        Ok(msg)
    }

    async fn handshake_send<I: AsyncWrite + Unpin>(stream: &mut I, buf: &[u8]) -> io::Result<()> {
        let len = u16::try_from(buf.len()).unwrap();
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(buf).await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct Secret {
    hash: [u8; 32],
    salt: [u8; 32],
}

impl Secret {
    pub fn new(password: &str, salt: Option<[u8; 32]>) -> Self {
        let salt: [u8; 32] = salt.unwrap_or_else(|| rand::rng().random());
        let mut hash = [0; 32];
        argon2::Argon2::default()
            .hash_password_into(password.as_bytes(), salt.as_slice(), &mut hash)
            .unwrap();
        Self { hash, salt }
    }
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }
    pub fn salt(&self) -> &[u8; 32] {
        &self.salt
    }
}
