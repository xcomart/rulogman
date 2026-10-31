//! The server half of a SOCKS handshake, for a dynamic forwarding.
//!
//! Only as much of SOCKS as `ssh -D` speaks: versions 5, 4 and 4a, no
//! authentication, and `CONNECT` alone — no `BIND`, no `UDP ASSOCIATE`. That is
//! what browsers, `curl --socks5-hostname` and database tools use, and every
//! other command is answered with the protocol's own "not supported" rather
//! than with a closed socket, so the client can say what went wrong.
//!
//! A host name the client sends is never resolved here. It is handed to the
//! SSH server as it arrived, which resolves it on the remote side — the whole
//! point of a proxy into a network whose names this machine does not know.

use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The only command answered: open a TCP connection.
const CMD_CONNECT: u8 = 0x01;

/// SOCKS5's "no authentication required" method.
const METHOD_NONE: u8 = 0x00;

/// SOCKS5's "no acceptable methods" answer to a greeting.
const METHOD_REFUSED: u8 = 0xFF;

/// Longest user id or host name a SOCKS4 request may carry before it is
/// refused, NUL included. SOCKS5 caps a name at 255 by its length byte; SOCKS4
/// has no cap, and a client streaming bytes without a NUL must not be read
/// forever.
const MAX_SOCKS4_FIELD: usize = 1024;

/// The protocol version a client spoke, which decides how it is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Version {
    /// SOCKS4, or SOCKS4a when the request carried a host name.
    Four,
    /// SOCKS5.
    Five,
}

/// Where a client asked to be connected, read off its request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    /// The version to answer in.
    pub version: Version,
    /// The host, as text for `direct-tcpip`: an address in its usual notation,
    /// or a name exactly as the client sent it.
    pub host: String,
    /// The TCP port.
    pub port: u16,
}

/// Why a handshake ended without a request to forward.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HandshakeError {
    /// The socket failed or closed part way through.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The first byte named no SOCKS version spoken here.
    #[error("not a SOCKS 4 or 5 client (first byte {0:#04x})")]
    Version(u8),
    /// A SOCKS5 client offered only authentication methods that need a
    /// password; it has been told so.
    #[error("the SOCKS client requires authentication, which is not offered")]
    NoAcceptableMethod,
    /// A command other than `CONNECT`; the client has been told it is not
    /// supported.
    #[error("SOCKS command {0:#04x} is not supported, only CONNECT")]
    Command(u8),
    /// A SOCKS5 address type that does not exist; the client has been told.
    #[error("SOCKS address type {0:#04x} is not supported")]
    AddressType(u8),
    /// A malformed request: a reserved byte that is not zero, an empty or
    /// non-text host name, a SOCKS4 field with no end.
    #[error("malformed SOCKS request: {0}")]
    Malformed(&'static str),
}

/// How the attempt to reach the requested host went, in the terms a SOCKS
/// reply can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The channel is open; data may flow.
    Connected,
    /// The SSH server refused by policy — `AllowTcpForwarding`, `PermitOpen`.
    NotAllowed,
    /// The SSH server could not connect to the host.
    Unreachable,
    /// Anything else.
    Failed,
}

/// Reads a client's greeting and request, answering the SOCKS5 method
/// negotiation on the way, and returns where it wants to go.
///
/// On success nothing has been sent in reply to the request itself yet: the
/// caller opens the channel first and then answers with [`reply`], since only
/// then is there an outcome to report. On an error that the protocol can
/// express, the client has already been answered and the socket may simply be
/// dropped.
pub(crate) async fn accept<S>(stream: &mut S) -> Result<Request, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match stream.read_u8().await? {
        0x05 => accept_five(stream).await,
        0x04 => accept_four(stream).await,
        other => Err(HandshakeError::Version(other)),
    }
}

/// SOCKS5, from just past the version byte of the greeting.
async fn accept_five<S>(stream: &mut S) -> Result<Request, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let count = stream.read_u8().await?;
    let mut methods = vec![0; usize::from(count)];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&METHOD_NONE) {
        stream.write_all(&[0x05, METHOD_REFUSED]).await?;
        return Err(HandshakeError::NoAcceptableMethod);
    }
    stream.write_all(&[0x05, METHOD_NONE]).await?;

    let mut head = [0; 4];
    stream.read_exact(&mut head).await?;
    let [version, command, reserved, address_type] = head;
    if version != 0x05 {
        return Err(HandshakeError::Malformed("the request is not SOCKS5"));
    }
    if reserved != 0 {
        return Err(HandshakeError::Malformed("the reserved byte is not zero"));
    }
    // The address is read before the command is judged, so that a refusal is
    // sent after the whole request has been taken off the wire — a client
    // that is still writing when it is answered may see a reset instead.
    let host = match address_type {
        0x01 => {
            let mut octets = [0; 4];
            stream.read_exact(&mut octets).await?;
            Ipv4Addr::from(octets).to_string()
        }
        0x03 => {
            let length = stream.read_u8().await?;
            let mut name = vec![0; usize::from(length)];
            stream.read_exact(&mut name).await?;
            host_name(name)?
        }
        0x04 => {
            let mut octets = [0; 16];
            stream.read_exact(&mut octets).await?;
            Ipv6Addr::from(octets).to_string()
        }
        other => {
            stream.write_all(&five_reply(0x08)).await?;
            return Err(HandshakeError::AddressType(other));
        }
    };
    let port = stream.read_u16().await?;
    if command != CMD_CONNECT {
        stream.write_all(&five_reply(0x07)).await?;
        return Err(HandshakeError::Command(command));
    }
    Ok(Request {
        version: Version::Five,
        host,
        port,
    })
}

/// SOCKS4 and 4a, from just past the version byte.
async fn accept_four<S>(stream: &mut S) -> Result<Request, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let command = stream.read_u8().await?;
    let port = stream.read_u16().await?;
    let mut octets = [0; 4];
    stream.read_exact(&mut octets).await?;
    // The user id means nothing without authentication; it is read only to
    // get past it.
    read_nul_terminated(stream).await?;
    // 0.0.0.x with x non-zero is SOCKS4a's sign that a host name follows.
    let host = if octets[..3] == [0, 0, 0] && octets[3] != 0 {
        host_name(read_nul_terminated(stream).await?)?
    } else {
        Ipv4Addr::from(octets).to_string()
    };
    if command != CMD_CONNECT {
        stream.write_all(&four_reply(false)).await?;
        return Err(HandshakeError::Command(command));
    }
    Ok(Request {
        version: Version::Four,
        host,
        port,
    })
}

/// Bytes up to (not including) the next NUL, refusing an endless field.
async fn read_nul_terminated<S>(stream: &mut S) -> Result<Vec<u8>, HandshakeError>
where
    S: AsyncRead + Unpin,
{
    let mut field = Vec::new();
    loop {
        match stream.read_u8().await? {
            0 => return Ok(field),
            byte if field.len() + 1 < MAX_SOCKS4_FIELD => field.push(byte),
            _ => return Err(HandshakeError::Malformed("a SOCKS4 field has no end")),
        }
    }
}

/// A host name off the wire, which has to be non-empty text.
fn host_name(bytes: Vec<u8>) -> Result<String, HandshakeError> {
    let name = String::from_utf8(bytes)
        .map_err(|_| HandshakeError::Malformed("the host name is not text"))?;
    if name.is_empty() {
        return Err(HandshakeError::Malformed("the host name is empty"));
    }
    Ok(name)
}

/// Answers a request read by [`accept`] with how reaching its host went.
pub(crate) async fn reply<S>(
    stream: &mut S,
    version: Version,
    outcome: Outcome,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match version {
        Version::Five => {
            let code = match outcome {
                Outcome::Connected => 0x00,
                Outcome::Failed => 0x01,
                Outcome::NotAllowed => 0x02,
                Outcome::Unreachable => 0x04,
            };
            stream.write_all(&five_reply(code)).await
        }
        Version::Four => {
            stream
                .write_all(&four_reply(outcome == Outcome::Connected))
                .await
        }
    }
}

/// A SOCKS5 reply with `code` and an all-zero IPv4 bound address.
///
/// The bound address would be the server's end of its connection to the
/// target, which `direct-tcpip` never reports; clients doing `CONNECT` do not
/// read it.
fn five_reply(code: u8) -> [u8; 10] {
    [0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

/// A SOCKS4 reply: granted (`0x5A`) or rejected (`0x5B`).
fn four_reply(granted: bool) -> [u8; 8] {
    [0x00, if granted { 0x5A } else { 0x5B }, 0, 0, 0, 0, 0, 0]
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    /// Runs [`accept`] against a client that writes `request` all at once, and
    /// returns the outcome with every byte the server sent back.
    async fn handshake(request: &[u8]) -> (Result<Request, HandshakeError>, Vec<u8>) {
        let (mut client, mut server) = duplex(4096);
        client.write_all(request).await.expect("write request");
        let result = accept(&mut server).await;
        drop(server);
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).await.expect("read answer");
        (result, answer)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
    }

    #[test]
    fn socks5_connect_to_a_name_keeps_the_name_for_the_server() {
        rt().block_on(async {
            let mut request = vec![0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x03, 11];
            request.extend_from_slice(b"db.internal");
            request.extend_from_slice(&5432u16.to_be_bytes());
            let (result, answer) = handshake(&request).await;
            assert_eq!(
                result.expect("accepted"),
                Request {
                    version: Version::Five,
                    host: "db.internal".to_owned(),
                    port: 5432,
                }
            );
            // Only the method choice so far: the request is answered once the
            // channel has been tried.
            assert_eq!(answer, [0x05, 0x00]);
        });
    }

    #[test]
    fn socks5_connect_to_ipv4_and_ipv6_addresses() {
        rt().block_on(async {
            let request = [0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x01, 10, 0, 0, 7, 0, 80];
            let (result, _) = handshake(&request).await;
            let request = result.expect("ipv4");
            assert_eq!((request.host.as_str(), request.port), ("10.0.0.7", 80));

            let mut request = vec![0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x04];
            request.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
            request.extend_from_slice(&443u16.to_be_bytes());
            let (result, _) = handshake(&request).await;
            let request = result.expect("ipv6");
            assert_eq!((request.host.as_str(), request.port), ("::1", 443));
        });
    }

    #[test]
    fn socks5_without_the_no_auth_method_is_refused() {
        rt().block_on(async {
            // Offers username/password (0x02) only.
            let (result, answer) = handshake(&[0x05, 0x01, 0x02]).await;
            assert!(matches!(result, Err(HandshakeError::NoAcceptableMethod)));
            assert_eq!(answer, [0x05, 0xFF]);
        });
    }

    #[test]
    fn socks5_bind_is_answered_as_unsupported() {
        rt().block_on(async {
            let request = [
                0x05, 0x01, 0x00, 0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0, 80,
            ];
            let (result, answer) = handshake(&request).await;
            assert!(matches!(result, Err(HandshakeError::Command(0x02))));
            assert_eq!(&answer[..2], [0x05, 0x00]);
            assert_eq!(answer[2..], five_reply(0x07));
        });
    }

    #[test]
    fn socks5_unknown_address_type_is_answered() {
        rt().block_on(async {
            let request = [0x05, 0x01, 0x00, 0x05, 0x01, 0x00, 0x09];
            let (result, answer) = handshake(&request).await;
            assert!(matches!(result, Err(HandshakeError::AddressType(0x09))));
            assert_eq!(answer[2..], five_reply(0x08));
        });
    }

    #[test]
    fn socks4_and_socks4a_requests() {
        rt().block_on(async {
            let mut request = vec![0x04, 0x01];
            request.extend_from_slice(&22u16.to_be_bytes());
            request.extend_from_slice(&[192, 168, 1, 2]);
            request.extend_from_slice(b"alice\0");
            let (result, answer) = handshake(&request).await;
            let parsed = result.expect("socks4");
            assert_eq!(parsed.version, Version::Four);
            assert_eq!((parsed.host.as_str(), parsed.port), ("192.168.1.2", 22));
            assert!(answer.is_empty());

            let mut request = vec![0x04, 0x01];
            request.extend_from_slice(&8080u16.to_be_bytes());
            request.extend_from_slice(&[0, 0, 0, 1]);
            request.extend_from_slice(b"\0intranet.example\0");
            let (result, _) = handshake(&request).await;
            let parsed = result.expect("socks4a");
            assert_eq!(
                (parsed.host.as_str(), parsed.port),
                ("intranet.example", 8080)
            );
        });
    }

    #[test]
    fn a_socks4_field_with_no_end_is_refused() {
        rt().block_on(async {
            let mut request = vec![0x04, 0x01, 0, 80, 1, 2, 3, 4];
            request.extend(std::iter::repeat_n(b'x', MAX_SOCKS4_FIELD + 10));
            let (result, _) = handshake(&request).await;
            assert!(matches!(result, Err(HandshakeError::Malformed(_))));
        });
    }

    #[test]
    fn anything_else_is_not_socks() {
        rt().block_on(async {
            let (result, answer) = handshake(b"GET / HTTP/1.1\r\n").await;
            assert!(matches!(result, Err(HandshakeError::Version(b'G'))));
            assert!(answer.is_empty());
        });
    }

    #[test]
    fn replies_carry_the_outcome_in_each_version() {
        rt().block_on(async {
            let mut written = Vec::new();
            reply(&mut written, Version::Five, Outcome::Connected)
                .await
                .expect("write");
            reply(&mut written, Version::Five, Outcome::NotAllowed)
                .await
                .expect("write");
            reply(&mut written, Version::Four, Outcome::Unreachable)
                .await
                .expect("write");
            assert_eq!(written[..10], five_reply(0x00));
            assert_eq!(written[10..20], five_reply(0x02));
            assert_eq!(written[20..], four_reply(false));
        });
    }
}
