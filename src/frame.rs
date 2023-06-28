use anyhow::{anyhow, bail};
use bytes::{BufMut, BytesMut};

use std::borrow::Cow;

use crate::{AckMode, FromServer, Message, Result, ToServer};

type HeaderTuple<'a> = (&'a [u8], Option<Cow<'a, [u8]>>);

#[derive(Debug, PartialEq)]
pub(crate) struct Frame<'a> {
    command: &'a [u8],
    // TODO use ArrayVec to keep headers on the stack
    // (makes this object zero-allocation)
    headers: Vec<(&'a [u8], Cow<'a, [u8]>)>,
    body: Option<&'a [u8]>,
}

impl<'a> Frame<'a> {
    pub(crate) fn new(
        command: &'a [u8],
        headers: &[HeaderTuple<'a>],
        body: Option<&'a [u8]>,
    ) -> Frame<'a> {
        let headers = headers
            .iter()
            // filter out headers with None value
            .filter_map(|&(k, ref v)| v.as_ref().map(|i| (k, i.clone())))
            .collect();
        Frame {
            command,
            headers,
            body,
        }
    }

    pub(crate) fn serialize(&self, buffer: &mut BytesMut) {
        fn write_escaped(b: u8, buffer: &mut BytesMut) {
            match b {
                b'\r' => buffer.put_slice(b"\\r"),
                b'\n' => buffer.put_slice(b"\\n"),
                b':' => buffer.put_slice(b"\\c"),
                b'\\' => buffer.put_slice(b"\\\\"),
                b => buffer.put_u8(b),
            }
        }
        let requires = self.command.len()
            + self.body.map(|b| b.len() + 20).unwrap_or(0)
            + self
                .headers
                .iter()
                .fold(0, |acc, (k, v)| acc + k.len() + v.len())
            + 30;
        if buffer.remaining_mut() < requires {
            buffer.reserve(requires);
        }
        buffer.put_slice(self.command);
        buffer.put_u8(b'\n');
        self.headers.iter().for_each(|&(key, ref val)| {
            for byte in key {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b':');
            for byte in val.iter() {
                write_escaped(*byte, buffer);
            }
            buffer.put_u8(b'\n');
        });
        buffer.put_u8(b'\n');
        if let Some(body) = self.body {
            buffer.put_slice(body);
        }
        buffer.put_u8(b'\x00');
    }
}

// nom7
use nom7::branch::alt;
use nom7::bytes::complete::tag;
use nom7::bytes::complete::take;
use nom7::bytes::complete::take_while;
use nom7::bytes::complete::{is_a, is_not};
use nom7::character::is_alphanumeric;
use nom7::character::streaming::line_ending;
use nom7::error::Error;
use nom7::multi::many_till;
use nom7::sequence::pair;
use nom7::sequence::tuple;
use nom7::sequence::Tuple;
use nom7::IResult;
use nom7::Parser;

fn nom7_eol(i: &[u8]) -> IResult<&[u8], &[u8]> {
    line_ending(i)
}

fn nom7_split_header(i: &[u8]) -> IResult<&[u8], (&[u8], &[u8], &[u8], &[u8])> {
    tuple((is_a("\r\n"), is_not(":"), take(1usize), is_not("\r\n"))).parse(i)
}

pub(crate) fn nom7_parse_frame(input: &[u8]) -> Frame {
    println!("Frame Parser:");
    // first word until new line
    let command = take_while(is_alphanumeric);
    // double new line -> end of headers
    let end_of_headers = pair(nom7_eol, nom7_eol);
    // \x00 -> end of file
    let end_of_file = pair(nom7_eol, tag("\x00"));
    // parse all headers into tuple
    let all_headers = many_till(nom7_split_header, alt((end_of_headers, end_of_file)));

    let (rest, (parsed_command, (headers_tuple, (_, _)))) =
        (&command, all_headers).parse(input).unwrap();

    println!(
        "command: {:?}",
        std::str::from_utf8(parsed_command).unwrap()
    );

    let p_headers: Vec<(&[u8], Cow<'_, [u8]>)> = headers_tuple
        .clone()
        .into_iter()
        .map(|(_, k, _, v)| ((k, Cow::Borrowed(v))))
        .collect();

    println!("Headers:");
    for (k, v) in &p_headers {
        println!(
            "  {:?}:{:?}",
            std::str::from_utf8(k),
            std::str::from_utf8(&v)
        )
    }

    let body_length = match p_headers.iter().position(|(k, _)| k == b"content-length") {
        Some(index) => match p_headers.get(index) {
            Some((_, b)) => Some(
                std::str::from_utf8(b.as_ref())
                    .unwrap()
                    .parse::<usize>()
                    .unwrap(),
            ),
            None => None,
        },
        None => None,
    };

    // exit on first \x00 unless body_length is set
    let body = match body_length {
        Some(l) => take::<_, _, Error<_>>(l)(rest),
        None => is_not("\x00")(rest),
    };

    let parsed_body = match body {
        Ok((_rest, bd)) => Some(bd),
        Err(_) => None,
    };
    println!(
        "Body: {:?}",
        std::str::from_utf8(parsed_body.unwrap_or(b"")).unwrap()
    );

    Frame {
        command: parsed_command,
        headers: p_headers,
        body: parsed_body,
    }
}

fn fetch_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Option<String> {
    let kk = key.as_bytes();
    for &(k, ref v) in headers {
        if k == kk {
            return String::from_utf8(v.to_vec()).ok();
        }
    }
    None
}

fn all_headers<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)]) -> Vec<(String, String)> {
    let mut res = Vec::new();
    for &(k, ref v) in headers {
        let entry = (
            String::from_utf8(k.to_vec()).unwrap(),
            String::from_utf8(v.to_vec()).unwrap(),
        );
        res.push(entry);
    }
    res
}

fn optional_headers<'a>(
    headers: &'a [(&'a [u8], Cow<'a, [u8]>)],
    expected_keys: &[&[u8]],
) -> Option<Vec<(String, String)>> {
    let res: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !expected_keys.contains(k))
        .map(|(k, v)| {
            (
                String::from_utf8(k.to_vec()).unwrap(),
                String::from_utf8(v.to_vec()).unwrap(),
            )
        })
        .collect();
    if res.is_empty() {
        None
    } else {
        Some(res)
    }
}

fn expect_header<'a>(headers: &'a [(&'a [u8], Cow<'a, [u8]>)], key: &'a str) -> Result<String> {
    fetch_header(headers, key).ok_or_else(|| anyhow!("Expected header '{}' missing", key))
}

impl<'a> Frame<'a> {
    #[allow(dead_code)]
    pub(crate) fn to_client_msg(&'a self) -> Result<Message<ToServer>> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use ToServer::*;
        let h = &self.headers;
        let expect_keys: &[&[u8]];
        let content = match self.command {
            b"STOMP" | b"CONNECT" | b"stomp" | b"connect" => {
                expect_keys = &[
                    b"accept-version",
                    b"host",
                    b"login",
                    b"passcode",
                    b"heart-beat",
                ];
                let heartbeat = if let Some(hb) = fh(h, "heart-beat") {
                    Some(parse_heartbeat(&hb)?)
                } else {
                    None
                };
                Connect {
                    accept_version: eh(h, "accept-version")?,
                    host: eh(h, "host")?,
                    login: fh(h, "login"),
                    passcode: fh(h, "passcode"),
                    heartbeat,
                }
            }
            b"DISCONNECT" | b"disconnect" => {
                expect_keys = &[b"receipt"];
                Disconnect {
                    receipt: fh(h, "receipt"),
                }
            }
            b"SEND" | b"send" => {
                expect_keys = &[b"destination", b"transaction"];
                Send {
                    destination: eh(h, "destination")?,
                    transaction: fh(h, "transaction"),
                    headers: optional_headers(h, expect_keys),
                    body: self.body.map(|v| v.to_vec()),
                }
            }
            b"SUBSCRIBE" | b"subscribe" => {
                expect_keys = &[b"destination", b"id", b"ack"];
                Subscribe {
                    destination: eh(h, "destination")?,
                    id: eh(h, "id")?,
                    ack: match fh(h, "ack").as_deref() {
                        Some("auto") => Some(AckMode::Auto),
                        Some("client") => Some(AckMode::Client),
                        Some("client-individual") => Some(AckMode::ClientIndividual),
                        Some(other) => bail!("Invalid ack mode: {}", other),
                        None => None,
                    },
                }
            }
            b"UNSUBSCRIBE" | b"unsubscribe" => {
                expect_keys = &[b"id"];
                Unsubscribe { id: eh(h, "id")? }
            }
            b"ACK" | b"ack" => {
                expect_keys = &[b"id", b"transaction"];
                Ack {
                    id: eh(h, "id")?,
                    transaction: fh(h, "transaction"),
                }
            }
            b"NACK" | b"nack" => {
                expect_keys = &[b"id", b"transaction"];
                Nack {
                    id: eh(h, "id")?,
                    transaction: fh(h, "transaction"),
                }
            }
            b"BEGIN" | b"begin" => {
                expect_keys = &[b"transaction"];
                Begin {
                    transaction: eh(h, "transaction")?,
                }
            }
            b"COMMIT" | b"commit" => {
                expect_keys = &[b"transaction"];
                Commit {
                    transaction: eh(h, "transaction")?,
                }
            }
            b"ABORT" | b"abort" => {
                expect_keys = &[b"transaction"];
                Abort {
                    transaction: eh(h, "transaction")?,
                }
            }
            other => bail!("Frame not recognized: {:?}", String::from_utf8_lossy(other)),
        };
        let extra_headers = h
            .iter()
            .filter_map(|&(k, ref v)| {
                if !expect_keys.contains(&k) {
                    Some((k.to_vec(), (v).to_vec()))
                } else {
                    None
                }
            })
            .collect();
        Ok(Message {
            content,
            extra_headers,
        })
    }

    pub(crate) fn to_server_msg(&'a self) -> Result<Message<FromServer>> {
        use self::expect_header as eh;
        use self::fetch_header as fh;
        use FromServer::{Connected, Error, Message as Msg, Receipt};
        let h = &self.headers;
        let expect_keys: &[&[u8]];
        let content = match self.command {
            b"CONNECTED" | b"connected" => {
                expect_keys = &[b"version", b"session", b"server", b"heart-beat"];
                Connected {
                    version: eh(h, "version")?,
                    session: fh(h, "session"),
                    server: fh(h, "server"),
                    heartbeat: fh(h, "heart-beat"),
                }
            }
            b"MESSAGE" | b"message" => {
                expect_keys = &[b"destination", b"message-id", b"subscription"];
                Msg {
                    destination: eh(h, "destination")?,
                    message_id: eh(h, "message-id")?,
                    subscription: eh(h, "subscription")?,
                    headers: all_headers(h),
                    body: self.body.map(|v| v.to_vec()),
                }
            }
            b"RECEIPT" | b"receipt" => {
                expect_keys = &[b"receipt-id"];
                Receipt {
                    receipt_id: eh(h, "receipt-id")?,
                }
            }
            b"ERROR" | b"error" => {
                expect_keys = &[b"message"];
                Error {
                    message: fh(h, "message"),
                    body: self.body.map(|v| v.to_vec()),
                }
            }
            other => bail!("Frame not recognized: {:?}", String::from_utf8_lossy(other)),
        };
        let extra_headers = h
            .iter()
            .filter_map(|&(k, ref v)| {
                if !expect_keys.contains(&k) {
                    Some((k.to_vec(), (v).to_vec()))
                } else {
                    None
                }
            })
            .collect();
        Ok(Message {
            content,
            extra_headers,
        })
    }
}

fn opt_str_to_bytes(s: &Option<String>) -> Option<Cow<'_, [u8]>> {
    s.as_ref().map(|v| Cow::Borrowed(v.as_bytes()))
}

fn parse_heartbeat(hb: &str) -> Result<(u32, u32)> {
    let mut split = hb.splitn(2, ',');
    let left = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    let right = split.next().ok_or_else(|| anyhow!("Bad heartbeat"))?;
    Ok((left.parse()?, right.parse()?))
}

impl ToServer {
    pub(crate) fn to_frame(&self) -> Frame {
        use self::opt_str_to_bytes as sb;
        use Cow::*;
        use ToServer::*;
        match *self {
            Connect {
                ref accept_version,
                ref host,
                ref login,
                ref passcode,
                ref heartbeat,
            } => Frame::new(
                b"CONNECT",
                &[
                    (b"accept-version", Some(Borrowed(accept_version.as_bytes()))),
                    (b"host", Some(Borrowed(host.as_bytes()))),
                    (b"login", sb(login)),
                    (
                        b"heart-beat",
                        heartbeat.map(|(v1, v2)| Owned(format!("{},{}", v1, v2).into())),
                    ),
                    (b"passcode", sb(passcode)),
                ],
                None,
            ),
            Disconnect { ref receipt } => {
                Frame::new(b"DISCONNECT", &[(b"receipt", sb(receipt))], None)
            }
            Subscribe {
                ref destination,
                ref id,
                ref ack,
            } => Frame::new(
                b"SUBSCRIBE",
                &[
                    (b"destination", Some(Borrowed(destination.as_bytes()))),
                    (b"id", Some(Borrowed(id.as_bytes()))),
                    (
                        b"ack",
                        ack.map(|ack| match ack {
                            AckMode::Auto => Borrowed(&b"auto"[..]),
                            AckMode::Client => Borrowed(&b"client"[..]),
                            AckMode::ClientIndividual => Borrowed(&b"client-individual"[..]),
                        }),
                    ),
                ],
                None,
            ),
            Unsubscribe { ref id } => Frame::new(
                b"UNSUBSCRIBE",
                &[(b"id", Some(Borrowed(id.as_bytes())))],
                None,
            ),
            Send {
                ref destination,
                ref transaction,
                ref headers,
                ref body,
            } => {
                let mut hdr: Vec<HeaderTuple> = vec![
                    (b"destination", Some(Borrowed(destination.as_bytes()))),
                    (b"id", sb(transaction)),
                ];
                if headers.is_some() {
                    for (key, val) in headers.as_ref().unwrap() {
                        hdr.push((key.as_bytes(), Some(Borrowed(val.as_bytes()))));
                    }
                }
                Frame::new(b"SEND", &hdr, body.as_ref().map(|v| v.as_ref()))
            }
            Ack {
                ref id,
                ref transaction,
            } => Frame::new(
                b"ACK",
                &[
                    (b"id", Some(Borrowed(id.as_bytes()))),
                    (b"transaction", sb(transaction)),
                ],
                None,
            ),
            Nack {
                ref id,
                ref transaction,
            } => Frame::new(
                b"NACK",
                &[
                    (b"id", Some(Borrowed(id.as_bytes()))),
                    (b"transaction", sb(transaction)),
                ],
                None,
            ),
            Begin { ref transaction } => Frame::new(
                b"BEGIN",
                &[(b"transaction", Some(Borrowed(transaction.as_bytes())))],
                None,
            ),
            Commit { ref transaction } => Frame::new(
                b"COMMIT",
                &[(b"transaction", Some(Borrowed(transaction.as_bytes())))],
                None,
            ),
            Abort { ref transaction } => Frame::new(
                b"ABORT",
                &[(b"transaction", Some(Borrowed(transaction.as_bytes())))],
                None,
            ),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// For all Frames Client -> Server
    /// https://stomp.github.io/stomp-specification-1.2.html#Client_Frames
    fn parse_and_serialize_to_server(
        data: &[u8],
        frame: Frame<'_>,
        headers_expect: Vec<(&[u8], &[u8])>,
        body_expect: Option<&[u8]>,
    ) {
        let fh: Vec<(&[u8], &[u8])> = frame.headers.iter().map(|&(k, ref v)| (k, &**v)).collect();
        println!("Provided Headers: ");
        for f in &fh {
            println!(
                "  {}: {}",
                std::str::from_utf8(f.0).unwrap(),
                std::str::from_utf8(f.1).unwrap()
            );
        }
        println!("Expected Headers: ");
        for f in &headers_expect {
            println!(
                "  {}: {}",
                std::str::from_utf8(f.0).unwrap(),
                std::str::from_utf8(f.1).unwrap()
            );
        }

        println!("Provided Body: ");
        println!(
            "{}",
            std::str::from_utf8(frame.body.unwrap_or(b"")).unwrap()
        );
        println!("Expected Body: ");
        println!(
            "{}",
            std::str::from_utf8(body_expect.unwrap_or(b"")).unwrap()
        );
        assert_eq!(fh, headers_expect, "headers dont match");
        assert_eq!(frame.body, body_expect, "body doesnt match");
        let stomp = frame.to_client_msg().unwrap();
        let mut buffer = BytesMut::new();
        stomp.to_frame().serialize(&mut buffer);
        println!("left: {}", std::str::from_utf8(&buffer).unwrap());
        println!("right: {}", std::str::from_utf8(&data).unwrap());
        assert_eq!(&*buffer, &*data, "frame data doesnt match");
    }

    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#CONNECT
    /// without heartbeat configured
    fn parse_and_serialize_client_connect_with_heartbeat() {
        let data = b"CONNECT
accept-version:1.2
host:datafeeds.here.co.uk
login:user
heart-beat:6,7
passcode:password\n\n\x00"
            .to_vec();
        let frame = nom7_parse_frame(&data);
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"heart-beat", b"6,7"),
            (b"passcode", b"password"),
        ];

        assert_eq!(frame.command, b"CONNECT");
        parse_and_serialize_to_server(&data, frame, headers_expect, None);
    }

    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#CONNECT
    /// with heartbeat configured
    fn parse_and_serialize_client_connect_without_heartbeat() {
        let data = b"CONNECT
accept-version:1.2
host:datafeeds.here.co.uk
login:user
passcode:password\n\n\x00";
        let frame = nom7_parse_frame(data);
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"accept-version"[..], &b"1.2"[..]),
            (b"host", b"datafeeds.here.co.uk"),
            (b"login", b"user"),
            (b"passcode", b"password"),
        ];

        assert_eq!(frame.command, b"CONNECT");
        parse_and_serialize_to_server(data, frame, headers_expect, None);
    }

    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#DISCONNECT
    fn parse_and_serialize_client_disconnect() {
        let data = b"DISCONNECT\nreceipt:77\n\n\x00";
        let frame = nom7_parse_frame(data);
        let headers_expect: Vec<(&[u8], &[u8])> = vec![(b"receipt", b"77")];

        assert_eq!(frame.command, b"DISCONNECT");
        parse_and_serialize_to_server(data, frame, headers_expect, None);
    }

    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#SEND
    /// minimum headers set without
    /// note: first \x00 will terminate body without content-length header!
    /// https://stomp.github.io/stomp-specification-1.2.html#Header_content-length
    fn parse_and_serialize_client_send_message_minimum() {
        let mut data = b"SEND\ndestination:/queue/a\n\n".to_vec();
        let body = b"this body contains no nulls \n and \n newlines OK?";
        data.extend_from_slice(body);
        data.extend_from_slice(b"\x00");
        let frame = nom7_parse_frame(&data);
        let headers_expect: Vec<(&[u8], &[u8])> = vec![(&b"destination"[..], &b"/queue/a"[..])];
        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body));
    }

    #[test]
    /// Testing:
    /// https://stomp.github.io/stomp-specification-1.2.html#SEND
    /// recommended headers set
    /// note: additional \x00 are only allowed with content-length header!
    /// https://stomp.github.io/stomp-specification-1.2.html#Header_content-length
    fn parse_and_serialize_client_send_message_recommended() {
        let mut data =
            b"SEND\ndestination:/queue/a\ncontent-type:text/html;charset=utf-8\n".to_vec();
        let body = "this body contains \x00 nulls \n and \r\n newlines \x00 OK?";
        let rest = format!("content-length:{}\n\n{}\x00", body.len(), body);
        data.extend_from_slice(rest.as_bytes());
        let frame = nom7_parse_frame(&data);
        let headers_expect: Vec<(&[u8], &[u8])> = vec![
            (&b"destination"[..], &b"/queue/a"[..]),
            (b"content-type", b"text/html;charset=utf-8"),
            (b"content-length", b"50"),
        ];

        assert_eq!(frame.command, b"SEND");
        parse_and_serialize_to_server(&data, frame, headers_expect, Some(body.as_bytes()));
    }

    #[test]
    fn test_nom7_normal_return() {
        let data = b"SEND
destination:datafeeds.here.co.uk\r\nmessage-id:12345
content-length:14
subscription:some-id

\n\nhallo\n\x00 Alex\x00";
        let nom7_frame = nom7_parse_frame(data);
        logging_nom(&nom7_frame);
    }

    #[test]
    fn test_nom7_early_body_return() {
        let data = b"SEND
destination:datafeeds.here.co.uk\r\nmessage-id:12345
subscription:some-id

\n\nhallo\n\x00 Alex\x00";
        let nom7_frame = nom7_parse_frame(data);
        logging_nom(&nom7_frame);
    }

    #[test]
    fn test_nom7_escapes() {
        let data = b"SEND
destination:data\\n\\r\\c\\feeds.here.co.uk\r\nmessage-id:12345
subscription:some-id

\n\nhallo\n\x00 Alex\x00";
        let nom7_frame = nom7_parse_frame(data);
        logging_nom(&nom7_frame);
    }

    #[test]
    fn test_nom7_empty() {
        let data = b"SEND

\n\nhallo\n\x00 Alex
\x00";
        let nom7_frame = nom7_parse_frame(data);
        logging_nom(&nom7_frame);
    }

    #[test]
    fn test_nom7_small_fails_nom_4() {
        let data = b"DISCONNECT
receipt:77
\x00";
        let nom7_frame = nom7_parse_frame(data);
        logging_nom(&nom7_frame);
    }

    fn logging_nom(frame: &Frame) {
        println!("Frame Struct:");
        println!("command: {:?}", std::str::from_utf8(frame.command));
        println!("Headers:");
        for (k, v) in &frame.headers {
            println!(
                "  {:?}:{:?}",
                std::str::from_utf8(k),
                std::str::from_utf8(&v)
            );
        }
        println!("Body:");
        println!("{:?}", std::str::from_utf8(frame.body.unwrap_or(b"")));
    }
}
