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

impl Noise {
    pub async fn host<I>(
        stream: &mut I,
        client_addr: impl Display,
        secret: &Secret,
    ) -> anyhow::Result<Self>
    where
        I: AsyncRead + AsyncWrite + Unpin,
    {
        debug!("New incoming connection from {client_addr}, starting secure handshake...");
        let mut buf = vec![0; 65536];

        Self::version_check(stream).await?;
        Self::handshake_send(stream, secret.salt()).await?;
        let mut handshake = Self::init_handshake(secret.hash(), false)?;

        match handshake.read_message(&Self::handshake_recv(stream).await?, &mut buf) {
            Ok(x) => x,
            Err(snow::Error::Decrypt) => {
                anyhow::bail!(
                    "Decryption failed during handshake. Are you sure the passwords match?"
                );
            }
            Err(e) => return Err(e.into()),
        };

        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;
        debug!("Handshake with {client_addr} complete!");

        Ok(Self {
            transport: handshake.into_transport_mode()?,
            buf,
        })
    }

    pub async fn satellite<I>(stream: &mut I, secret: &str) -> anyhow::Result<Self>
    where
        I: AsyncRead + AsyncWrite + Unpin,
    {
        let mut buf = vec![0; 65536];
        debug!("Established connection, starting secure handshake...");
        Self::version_check(stream).await?;
        let salt: [u8; 32] = Self::handshake_recv(stream).await?.try_into().unwrap();
        let secret = Secret::new(secret, Some(salt));
        let mut handshake = Self::init_handshake(secret.hash(), true)?;

        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;

        let recv_bytes = match Self::handshake_recv(stream).await {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                anyhow::bail!("Server dropped the connection mid-handshake. Are you sure the passwords match?");
            }
            Err(e) => return Err(e.into()),
        };
        handshake.read_message(recv_bytes.as_slice(), &mut buf)?;
        debug!("Handshake complete!");

        Ok(Self {
            transport: handshake.into_transport_mode()?,
            buf,
        })
    }

    pub fn encode_message(&mut self, msg: &[u8]) -> anyhow::Result<&[u8]> {
        let len = self.transport.write_message(msg, &mut self.buf)?;
        Ok(&self.buf[..len])
    }

    pub fn decode_message(&mut self, msg: &[u8]) -> anyhow::Result<&[u8]> {
        let len = self.transport.read_message(msg, &mut self.buf)?;
        Ok(&self.buf[..len])
    }

    fn init_handshake(hash: &[u8; 32], initiator: bool) -> Result<HandshakeState, snow::Error> {
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

    async fn version_check<I: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut I,
    ) -> anyhow::Result<()> {
        let version = env!("CARGO_PKG_VERSION");
        Self::handshake_send(stream, version.as_bytes()).await?;
        let peer_bytes = Self::handshake_recv(stream).await?;
        let peer_version = String::from_utf8_lossy(&peer_bytes);
        if version != peer_version {
            anyhow::bail!(
                "Handshake failed: peer version (v{peer_version}) doesn't match mine (v{version})"
            );
        }

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
