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

// workflow 1:
// a: handshake.write_message (encrypt) &str clipboard into buf1
// b: cobs::encode that into buf2
// c: push 0
// d: write_all into TcpStream
// e: clear

// workflow 2:
// a: read until 0 into buf1 (what about partial?)
// b: cobs decode in place, pop 0
// c: handshake.read_message into buf2

pub struct Noise {
    transport: TransportState,
    buf: Vec<u8>,
}

impl Noise {
    pub async fn host(
        stream: &mut TcpStream,
        client_addr: &SocketAddr,
        secret: &str,
    ) -> anyhow::Result<Self> {
        let mut buf = vec![0; 65536];
        let mut handshake = Self::init_handshake(secret, false)?;

        debug!(%client_addr, "New connection, starting secure handshake");
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

    pub async fn satellite(stream: &mut TcpStream, secret: &str) -> anyhow::Result<Self> {
        let mut buf = vec![0; 65536];
        let mut handshake = Self::init_handshake(secret, true)?;

        debug!("Established connection, starting secure handshake");
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

    fn init_handshake(secret: &str, initiator: bool) -> anyhow::Result<HandshakeState> {
        const PSK_LEN: usize = 32;
        let secret: Vec<u8> = secret.bytes().cycle().take(PSK_LEN).collect();
        let builder = Builder::new(PARAMS.clone());
        let static_key = builder.generate_keypair().unwrap().private;
        let builder = builder
            .local_private_key(&static_key)
            .psk(3, secret.as_slice());
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
