use bytes::{Buf, BytesMut};
use futures::prelude::*;
use futures::sink::SinkExt;

use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed};
use typed_builder::TypedBuilder;

pub type ClientTransport = Framed<TcpStream, ClientCodec>;

use crate::frame;
use crate::{FromServer, Message, Result, ToServer};
use anyhow::{anyhow, bail};

/// Create a connection to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
///
/// `virtualhost` If no specific virtualhost is desired, it is recommended
/// to set this to the same as the host name that the socket
/// was established against (i.e, the same as the server address).
///
/// # Examples
///
/// ```rust,no_run
/// use tokio_stomp_2::client::Connector;
///
///#[tokio::main]
/// async fn main() {
///   let connection = Connector::builder()
///     .server("stomp.example.com")
///     .virtualhost("stomp.example.com")
///     .login(Some("guest".to_string()))
///     .passcode(Some("guest".to_string()))
///     .connect()
///     .await;
///}
/// ```
#[derive(TypedBuilder)]
#[builder(build_method(vis="", name=__build))]
pub struct Connector<S: tokio::net::ToSocketAddrs, V: Into<String>> {
    /// The address to the stomp server
    server: S,
    /// Virtualhost, if no specific virtualhost is desired, it is recommended
    /// to set this to the same as the host name that the socket
    virtualhost: V,
    /// Username to use for optional authentication to the server
    #[builder(default)]
    login: Option<String>,
    /// Passcode to use for optional authentication to the server
    #[builder(default)]
    passcode: Option<String>,
    /// Custom headers to be sent to the server
    #[builder(default)]
    headers: Vec<(String, String)>,
}

#[allow(non_camel_case_types)]
impl<
        S: tokio::net::ToSocketAddrs,
        V: Into<String>,
        __login: ::typed_builder::Optional<Option<String>>,
        __passcode: ::typed_builder::Optional<Option<String>>,
        __headers: ::typed_builder::Optional<Vec<(String, String)>>,
    > ConnectorBuilder<S, V, ((S,), (V,), __login, __passcode, __headers)>
{
    pub async fn connect(self) -> Result<ClientTransport> {
        let connector = self.__build();
        connector.connect().await
    }

    pub fn msg(self) -> Message<ToServer> {
        let connector = self.__build();
        connector.msg()
    }
}

impl<S: tokio::net::ToSocketAddrs, V: Into<String>> Connector<S, V> {
    pub async fn connect(self) -> Result<ClientTransport> {
        let tcp = TcpStream::connect(self.server).await?;
        let mut transport = ClientCodec.framed(tcp);
        client_handshake(
            &mut transport,
            self.virtualhost.into(),
            self.login,
            self.passcode,
            self.headers,
        )
        .await?;
        Ok(transport)
    }

    pub fn msg(self) -> Message<ToServer> {
        let extra_headers = self
            .headers
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        Message {
            content: ToServer::Connect {
                accept_version: "1.2".into(),
                host: self.virtualhost.into(),
                login: self.login,
                passcode: self.passcode,
                heartbeat: None,
            },
            extra_headers,
        }
    }
}

/// Connect to a STOMP server via TCP, including the connection handshake.
/// If successful, returns a tuple of a message stream and a sender,
/// which may be used to receive and send messages respectively.
///
/// `virtualhost` If no specific virtualhost is desired, it is recommended
/// to set this to the same as the host name that the socket
/// was established against (i.e, the same as the server address).
pub async fn connect(
    server: impl tokio::net::ToSocketAddrs,
    virtualhost: impl Into<String>,
    login: Option<String>,
    passcode: Option<String>,
) -> Result<ClientTransport> {
    Connector::builder()
        .server(server)
        .virtualhost(virtualhost)
        .login(login)
        .passcode(passcode)
        .connect()
        .await
}

async fn client_handshake(
    transport: &mut ClientTransport,
    virtualhost: String,
    login: Option<String>,
    passcode: Option<String>,
    headers: Vec<(String, String)>,
) -> Result<()> {
    let extra_headers = headers
        .iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    let connect = Message {
        content: ToServer::Connect {
            accept_version: "1.2".into(),
            host: virtualhost,
            login,
            passcode,
            heartbeat: None,
        },
        extra_headers,
    };
    // Send the message
    transport.send(connect).await?;
    // Receive reply
    let msg = transport.next().await.transpose()?;
    if let Some(FromServer::Connected { .. }) = msg.as_ref().map(|m| &m.content) {
        Ok(())
    } else {
        Err(anyhow!("unexpected reply: {:?}", msg))
    }
}

/// Builder to create a Subscribe message with optional custom headers

/// # Examples
///
/// ```rust,no_run
/// use futures::prelude::*;
/// use tokio_stomp_2::client::Connector;
/// use tokio_stomp_2::client::Subscriber;
///
///
/// #[tokio::main]
/// async fn main() -> Result<(), anyhow::Error> {
///   let mut connection = Connector::builder()
///     .server("stomp.example.com")
///     .virtualhost("stomp.example.com")
///     .login(Some("guest".to_string()))
///     .passcode(Some("guest".to_string()))
///     .headers(vec![("client-id".to_string(), "ClientTest".to_string())])
///     .connect()
///     .await.expect("Client connection");
///   
///   let subscribe_msg = Subscriber::builder()
///     .destination("queue.test")
///     .id("custom-subscriber-id")
///     .subscribe();
///
///   connection.send(subscribe_msg).await?;
///   Ok(())
/// }
/// ```
#[derive(TypedBuilder)]
#[builder(build_method(vis="", name=__build))]
pub struct Subscriber<S: Into<String>, I: Into<String>> {
    destination: S,
    id: I,
    #[builder(default)]
    headers: Vec<(String, String)>,
}

#[allow(non_camel_case_types)]
impl<
        S: Into<String>,
        I: Into<String>,
        __headers: ::typed_builder::Optional<Vec<(String, String)>>,
    > SubscriberBuilder<S, I, ((S,), (I,), __headers)>
{
    pub fn subscribe(self) -> Message<ToServer> {
        let subscriber = self.__build();
        subscriber.subscribe()
    }
}

impl<S: Into<String>, I: Into<String>> Subscriber<S, I> {
    pub fn subscribe(self) -> Message<ToServer> {
        let mut msg: Message<ToServer> = ToServer::Subscribe {
            destination: self.destination.into(),
            id: self.id.into(),
            ack: None,
        }
        .into();
        msg.extra_headers = self
            .headers
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();
        msg
    }
}

/// Convenience function to build a Subscribe message
pub fn subscribe(dest: impl Into<String>, id: impl Into<String>) -> Message<ToServer> {
    Subscriber::builder().destination(dest).id(id).subscribe()
}

pub struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = Message<FromServer>;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        let (item, offset) = match frame::parse_frame(src) {
            Ok((remain, frame)) => (
                Message::<FromServer>::from_frame(frame),
                remain.as_ptr() as usize - src.as_ptr() as usize,
            ),
            Err(nom::Err::Incomplete(_)) => return Ok(None),
            Err(e) => bail!("Parse failed: {:?}", e),
        };
        src.advance(offset);
        item.map(Some)
    }
}

impl Encoder<Message<ToServer>> for ClientCodec {
    type Error = anyhow::Error;

    fn encode(
        &mut self,
        item: Message<ToServer>,
        dst: &mut BytesMut,
    ) -> std::result::Result<(), Self::Error> {
        item.to_frame().serialize(dst);
        Ok(())
    }
}

#[test]
fn subscription_message() {
    let headers = vec![(
        "activemq.subscriptionName".to_string(),
        "ClientTest".to_string(),
    )];
    let subscribe_msg = Subscriber::builder()
        .destination("queue.test")
        .id("custom-subscriber-id")
        .headers(headers.clone())
        .subscribe();
    let mut expected: Message<ToServer> = ToServer::Subscribe {
        destination: "queue.test".to_string(),
        id: "custom-subscriber-id".to_string(),
        ack: None,
    }
    .into();
    expected.extra_headers = headers
        .into_iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();

    let mut expected_buffer = BytesMut::new();
    expected.to_frame().serialize(&mut expected_buffer);
    let mut actual_buffer = BytesMut::new();
    subscribe_msg.to_frame().serialize(&mut actual_buffer);

    assert_eq!(expected_buffer, actual_buffer);
}

#[test]
fn connection_message() {
    let headers = vec![("client-id".to_string(), "ClientTest".to_string())];
    let connect_msg = Connector::builder()
        .server("stomp.example.com")
        .virtualhost("virtual.stomp.example.com")
        .login(Some("guest_login".to_string()))
        .passcode(Some("guest_passcode".to_string()))
        .headers(headers.clone())
        .msg();

    let mut expected: Message<ToServer> = ToServer::Connect {
        accept_version: "1.2".into(),
        host: "virtual.stomp.example.com".into(),
        login: Some("guest_login".to_string()),
        passcode: Some("guest_passcode".to_string()),
        heartbeat: None,
    }
    .into();
    expected.extra_headers = headers
        .into_iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();

    let mut expected_buffer = BytesMut::new();
    expected.to_frame().serialize(&mut expected_buffer);
    let mut actual_buffer = BytesMut::new();
    connect_msg.to_frame().serialize(&mut actual_buffer);

    assert_eq!(expected_buffer, actual_buffer);
}
