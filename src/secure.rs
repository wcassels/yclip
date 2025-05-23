use rand::Rng;
use snow::{params::NoiseParams, Builder, HandshakeState, TransportState};
use std::{
    io::{self},
    net::SocketAddr,
    sync::LazyLock,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};
use tracing::*;

static PARAMS: LazyLock<NoiseParams> =
    LazyLock::new(|| "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s".parse().unwrap());

pub struct Noise {
    transport: TransportState,
    buf: Vec<u8>,
}

impl Noise {
    pub async fn host(
        stream: &mut TcpStream,
        client_addr: &SocketAddr,
        secret: &Secret,
    ) -> anyhow::Result<Self> {
        debug!(%client_addr, "New connection, starting secure handshake");
        let mut buf = vec![0; 65536];

        Self::handshake_send(stream, secret.salt()).await?;

        let mut handshake = Self::init_handshake(secret.hash(), false)?;

        handshake.read_message(&Self::handshake_recv(stream).await?, &mut buf)?;
        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;
        handshake.read_message(&Self::handshake_recv(stream).await?, &mut buf)?;
        debug!(%client_addr, "Handshake complete");

        Ok(Self {
            transport: handshake.into_transport_mode()?,
            buf,
        })
    }

    pub async fn satellite(stream: &mut TcpStream, secret: String) -> anyhow::Result<Self> {
        let mut buf = vec![0; 65536];
        debug!("Established connection, starting secure handshake");
        let salt: [u8; 32] = Self::handshake_recv(stream).await?.try_into().unwrap();
        let secret = Secret::new(secret, Some(salt));
        let mut handshake = Self::init_handshake(secret.hash(), true)?;

        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;
        handshake.read_message(&Self::handshake_recv(stream).await?, &mut buf)?;
        let len = handshake.write_message(&[], &mut buf)?;
        Self::handshake_send(stream, &buf[..len]).await?;
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

    fn init_handshake(hash: &[u8; 32], initiator: bool) -> anyhow::Result<HandshakeState> {
        let builder = Builder::new(PARAMS.clone());
        let static_key = builder.generate_keypair().unwrap().private;
        let builder = builder.local_private_key(&static_key).psk(3, hash);
        if initiator {
            Ok(builder.build_initiator()?)
        } else {
            Ok(builder.build_responder()?)
        }
    }

    async fn handshake_recv(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        let mut msg_len_buf = [0_u8; 2];
        stream.read_exact(&mut msg_len_buf).await?;
        let msg_len = usize::from(u16::from_be_bytes(msg_len_buf));
        let mut msg = vec![0_u8; msg_len];
        stream.read_exact(&mut msg[..]).await?;
        Ok(msg)
    }

    async fn handshake_send(stream: &mut TcpStream, buf: &[u8]) -> anyhow::Result<()> {
        let len = u16::try_from(buf.len())?;
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
    pub fn new(password: String, salt: Option<[u8; 32]>) -> Self {
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
