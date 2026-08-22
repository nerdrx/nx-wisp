//! A small, exact RFC 6455 client — the Rust half of `nx-connector.js`.
//!
//! The bus is "a plain WebSocket, no subprotocol, no extensions, text frames
//! only, 16 KB cap" (PROTOCOL.md §1), which is little enough that hand-rolling
//! it costs less than a dependency and keeps the whole wire visible in one
//! file. The framing here is deliberately symmetric — it can also speak the
//! *server* half — because that is what lets the test suite stand up a mock bus
//! over an in-memory pipe and never need a running NX Hub.
//!
//! Two things this file will not compromise on, because they are what tells the
//! real hub apart from some unrelated program squatting on port 9021:
//!
//! * `Sec-WebSocket-Accept` is verified, not assumed.
//! * A server frame that arrives masked is a protocol violation, and a client
//!   frame that arrives unmasked is too (RFC 6455 §5.1).

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The hub's cap, applied to a whole reassembled message.
pub const MAX_MESSAGE: usize = 16 * 1024;
const MAX_HEADER: usize = 8192;

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xa;

#[derive(Debug, Error)]
pub enum WsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("handshake: {0}")]
    Handshake(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("message over {MAX_MESSAGE} bytes")]
    TooLarge,
    #[error("peer closed the connection")]
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<u16>),
}

/// Which side of the connection we are, which decides who must mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// We mask what we send; anything we receive must be unmasked.
    Client,
    /// We do not mask; anything we receive must be masked.
    Server,
}

impl Side {
    fn we_mask(self) -> bool {
        matches!(self, Side::Client)
    }
    fn peer_masks(self) -> bool {
        matches!(self, Side::Server)
    }
}

/// 4 bytes of masking key / 16 bytes of handshake nonce, straight from the
/// kernel. No RNG dependency, and no userspace state to get forked.
fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut out = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    // /dev/urandom is not optional on Linux, but a masking key that is merely
    // unique is still better than a panic on a loopback socket.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32;
    let mut x = seed | 1;
    for byte in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = (x >> 24) as u8;
    }
    out
}

/// `n` random bytes as lowercase hex — hop ids, and nothing security-critical.
pub(crate) fn random_hex(n: usize) -> String {
    random_bytes(n).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn accept_key(client_key: &str) -> String {
    let mut h = Sha1::new();
    h.update(client_key.as_bytes());
    h.update(WS_GUID.as_bytes());
    B64.encode(h.finalize())
}

fn encode_frame(opcode: u8, payload: &[u8], mask: bool) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(len + 14);
    out.push(0x80 | opcode); // FIN, no RSV
    let mask_bit = if mask { 0x80 } else { 0x00 };
    if len < 126 {
        out.push(mask_bit | len as u8);
    } else if len < 65536 {
        out.push(mask_bit | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if mask {
        let key = random_bytes(4);
        out.extend_from_slice(&key);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i & 3]));
    } else {
        out.extend_from_slice(payload);
    }
    out
}

/// The write half. Cheap to hold; every send is one syscall's worth of bytes.
pub struct WsWriter<W> {
    inner: W,
    side: Side,
}

impl<W: AsyncWrite + Unpin> WsWriter<W> {
    pub fn new(inner: W, side: Side) -> Self {
        Self { inner, side }
    }

    pub async fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        self.send(OP_TEXT, text.as_bytes()).await
    }

    pub async fn send_ping(&mut self, payload: &[u8]) -> Result<(), WsError> {
        self.send(OP_PING, payload).await
    }

    pub async fn send_pong(&mut self, payload: &[u8]) -> Result<(), WsError> {
        self.send(OP_PONG, payload).await
    }

    pub async fn send_close(&mut self, code: u16) -> Result<(), WsError> {
        self.send(OP_CLOSE, &code.to_be_bytes()).await
    }

    async fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        let frame = encode_frame(opcode, payload, self.side.we_mask());
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// The read half: frames in, whole messages out. Fragmentation and control
/// frames are handled here so callers only ever see a complete message.
pub struct WsReader<R> {
    inner: R,
    buf: Vec<u8>,
    frag: Option<(u8, Vec<u8>)>,
    side: Side,
    dead: bool,
}

impl<R: AsyncRead + Unpin> WsReader<R> {
    /// `leftover` is whatever the HTTP handshake read past the header boundary
    /// — a peer that pipelines its first frame behind the response is legal and
    /// the hub's own fleet code does exactly that.
    pub fn new(inner: R, leftover: Vec<u8>, side: Side) -> Self {
        Self { inner, buf: leftover, frag: None, side, dead: false }
    }

    pub async fn next(&mut self) -> Result<WsMessage, WsError> {
        loop {
            if self.dead {
                return Err(WsError::Closed);
            }
            if let Some(msg) = self.parse()? {
                return Ok(msg);
            }
            let mut chunk = [0u8; 4096];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                self.dead = true;
                return Err(WsError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// One pass over the buffer. `Ok(None)` means "need more bytes".
    fn parse(&mut self) -> Result<Option<WsMessage>, WsError> {
        loop {
            if self.buf.len() < 2 {
                return Ok(None);
            }
            let fin = self.buf[0] & 0x80 != 0;
            let opcode = self.buf[0] & 0x0f;
            let masked = self.buf[1] & 0x80 != 0;
            let mut len = (self.buf[1] & 0x7f) as usize;
            let mut off = 2;
            if len == 126 {
                if self.buf.len() < 4 {
                    return Ok(None);
                }
                len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                off = 4;
            } else if len == 127 {
                if self.buf.len() < 10 {
                    return Ok(None);
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.buf[2..10]);
                let wide = u64::from_be_bytes(b);
                if wide > MAX_MESSAGE as u64 {
                    self.dead = true;
                    return Err(WsError::TooLarge);
                }
                len = wide as usize;
                off = 10;
            }
            if masked != self.side.peer_masks() {
                self.dead = true;
                return Err(WsError::Protocol(if masked {
                    "peer masked a frame it must not have".into()
                } else {
                    "peer sent an unmasked frame".into()
                }));
            }
            let held = self.frag.as_ref().map(|(_, b)| b.len()).unwrap_or(0);
            if len > MAX_MESSAGE || held + len > MAX_MESSAGE {
                self.dead = true;
                return Err(WsError::TooLarge);
            }
            let key_len = if masked { 4 } else { 0 };
            if self.buf.len() < off + key_len + len {
                return Ok(None);
            }
            let key = self.buf[off..off + key_len].to_vec();
            let start = off + key_len;
            let mut payload = self.buf[start..start + len].to_vec();
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= key[i & 3];
                }
            }
            self.buf.drain(..start + len);

            if opcode & 0x08 != 0 {
                // Control frames are never fragmented and never interleave with
                // a message in flight.
                return Ok(Some(match opcode {
                    OP_PING => WsMessage::Ping(payload),
                    OP_PONG => WsMessage::Pong(payload),
                    OP_CLOSE => {
                        self.dead = true;
                        let code = if payload.len() >= 2 {
                            Some(u16::from_be_bytes([payload[0], payload[1]]))
                        } else {
                            None
                        };
                        WsMessage::Close(code)
                    }
                    other => {
                        self.dead = true;
                        return Err(WsError::Protocol(format!("unknown control opcode {other}")));
                    }
                }));
            }

            match (opcode, fin) {
                (OP_CONT, _) => {
                    let Some((op, held)) = self.frag.as_mut() else {
                        self.dead = true;
                        return Err(WsError::Protocol("continuation without a start".into()));
                    };
                    held.extend_from_slice(&payload);
                    if fin {
                        let op = *op;
                        let (_, whole) = self.frag.take().expect("checked above");
                        return Ok(Some(Self::finish(op, whole)?));
                    }
                }
                (op, true) => return Ok(Some(Self::finish(op, payload)?)),
                (op, false) => {
                    self.frag = Some((op, std::mem::take(&mut payload)));
                }
            }
        }
    }

    fn finish(opcode: u8, payload: Vec<u8>) -> Result<WsMessage, WsError> {
        match opcode {
            OP_TEXT => String::from_utf8(payload)
                .map(WsMessage::Text)
                .map_err(|_| WsError::Protocol("text frame was not utf-8".into())),
            OP_BINARY => Ok(WsMessage::Binary(payload)),
            other => Err(WsError::Protocol(format!("unknown opcode {other}"))),
        }
    }
}

/// The client half of the HTTP upgrade. Returns any bytes read past the header.
pub async fn client_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
    resource: &str,
) -> Result<Vec<u8>, WsError> {
    let key = B64.encode(random_bytes(16));
    let expect = accept_key(&key);
    let req = format!(
        "GET {resource} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    let (head, rest) = read_head(stream).await?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if !status.starts_with("HTTP/1.1 101") {
        return Err(WsError::Handshake(format!("not an upgrade: {status}")));
    }
    let accept = header(&head, "sec-websocket-accept")
        .ok_or_else(|| WsError::Handshake("no Sec-WebSocket-Accept".into()))?;
    if accept != expect {
        // Wrong server, or a stale/foreign listener on the port.
        return Err(WsError::Handshake("Sec-WebSocket-Accept did not match".into()));
    }
    Ok(rest)
}

/// The server half, so the tests can be a bus.
pub async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<Vec<u8>, WsError> {
    let (head, rest) = read_head(stream).await?;
    if !head.starts_with("GET ") {
        return Err(WsError::Handshake("not a GET".into()));
    }
    let key = header(&head, "sec-websocket-key")
        .ok_or_else(|| WsError::Handshake("no Sec-WebSocket-Key".into()))?;
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await?;
    Ok(rest)
}

async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(String, Vec<u8>), WsError> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = find(&head, b"\r\n\r\n") {
            let rest = head[end + 4..].to_vec();
            head.truncate(end);
            return Ok((String::from_utf8_lossy(&head).into_owned(), rest));
        }
        if head.len() > MAX_HEADER {
            return Err(WsError::Handshake("header too long".into()));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(WsError::Closed);
        }
        head.extend_from_slice(&chunk[..n]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn header(head: &str, name: &str) -> Option<String> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_the_rfc_example() {
        // RFC 6455 §1.3.
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[tokio::test]
    async fn a_masked_client_frame_round_trips_to_a_server_reader() {
        let (a, b) = tokio::io::duplex(4096);
        let mut w = WsWriter::new(a, Side::Client);
        let mut r = WsReader::new(b, Vec::new(), Side::Server);
        w.send_text("{\"type\":\"hello\"}").await.unwrap();
        assert_eq!(r.next().await.unwrap(), WsMessage::Text("{\"type\":\"hello\"}".into()));
    }

    #[tokio::test]
    async fn an_unmasked_client_frame_is_a_protocol_error() {
        let (a, b) = tokio::io::duplex(4096);
        // Deliberately the wrong side: a client that forgot to mask.
        let mut w = WsWriter::new(a, Side::Server);
        let mut r = WsReader::new(b, Vec::new(), Side::Server);
        w.send_text("nope").await.unwrap();
        assert!(matches!(r.next().await, Err(WsError::Protocol(_))));
    }

    #[tokio::test]
    async fn fragmented_messages_reassemble_and_control_frames_pass_through() {
        let (mut a, b) = tokio::io::duplex(4096);
        let mut r = WsReader::new(b, Vec::new(), Side::Client);
        // "he" + "llo" as two unmasked server frames, with a ping between.
        a.write_all(&[0x01, 0x02, b'h', b'e']).await.unwrap();
        a.write_all(&[0x89, 0x00]).await.unwrap();
        a.write_all(&[0x80, 0x03, b'l', b'l', b'o']).await.unwrap();
        assert_eq!(r.next().await.unwrap(), WsMessage::Ping(Vec::new()));
        assert_eq!(r.next().await.unwrap(), WsMessage::Text("hello".into()));
    }

    #[tokio::test]
    async fn handshakes_agree_with_each_other() {
        let (client, server) = tokio::io::duplex(8192);
        let srv = tokio::spawn(async move {
            let mut server = server;
            server_handshake(&mut server).await.unwrap();
            server
        });
        let mut client = client;
        let rest = client_handshake(&mut client, "127.0.0.1", 9021, "/").await.unwrap();
        assert!(rest.is_empty());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn a_foreign_listener_fails_the_accept_check() {
        let (client, mut server) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = server.read(&mut buf).await;
            let _ = server
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                      Sec-WebSocket-Accept: not-the-right-thing\r\n\r\n",
                )
                .await;
        });
        let mut client = client;
        let err = client_handshake(&mut client, "127.0.0.1", 9021, "/").await.unwrap_err();
        assert!(matches!(err, WsError::Handshake(_)));
    }
}
