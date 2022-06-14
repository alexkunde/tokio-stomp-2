use std::net::ToSocketAddrs;

use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;

use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed};

type ClientTransport = Framed<TcpStream, ClientCodec>;

use crate::frame;

use crate::{FromServer, Message, Result, ToServer};

/// Connect to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
pub async fn connect(
    address: &str,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<
    impl Stream<Item = Result<Message<FromServer>>> + Sink<Message<ToServer>, Error = anyhow::Error>,
> {
    let addr = address.to_socket_addrs().unwrap().next().unwrap();
    let tcp = TcpStream::connect(&addr).await?;
    let mut transport = ClientCodec.framed(tcp);
    client_handshake(&mut transport, address.to_string(), login, passcode).await?;
    return Ok(transport);
}

async fn client_handshake(
    transport: &mut ClientTransport,
    host: String,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<()> {
    let connect = Message {
        content: ToServer::Connect {
            accept_version: String::from("1.2"),
            host: host,
            login: login,
            passcode: passcode,
            heartbeat: None,
        },
        extra_headers: vec![],
    };
    // Send the message
    transport.send(connect).await?;
    // Receive reply
    let msg = transport.next().await.transpose()?;
    if let Some(FromServer::Connected { .. }) = msg.as_ref().map(|m| &m.content) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Handshake error, unexpected reply: {:?}",
            msg
        ))
    }
}

/// Convenience function to build a Subscribe message
// #[allow(dead_code)]
pub fn subscribe(dest: &str, id: &str) -> Message<ToServer> {
    ToServer::Subscribe {
        destination: dest.into(),
        id: id.into(),
        ack: None,
    }
    .into()
}

struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = Message<FromServer>;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        let (item, offset) = match frame::parse_frame(&src) {
            Ok((remain, frame)) => (
                Message::<FromServer>::from_frame(frame),
                remain.as_ptr() as usize - src.as_ptr() as usize,
            ),
            Err(nom::Err::Incomplete(_)) => return Ok(None),
            Err(e) => anyhow::bail!("Parse failed: {:?}", e),
        };
        src.advance(offset);
        item.map(|v| Some(v))
    }
}

impl Encoder<Message<ToServer>> for ClientCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, item: Message<ToServer>, dst: &mut BytesMut) -> Result<()> {
        item.to_frame().serialize(dst);
        Ok(())
    }
}
