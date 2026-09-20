// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The WebSocket upgrade, and the pump that moves bytes between a browser and
//! a device session socket.
//!
//! # Why one thread and non-blocking sockets
//!
//! A relay has to move both directions at once, and the obvious shape — a
//! thread per direction — does not fit here. The device side may be a
//! `SslStream`, which cannot be split into an independent reader and writer,
//! and a `tungstenite::WebSocket` owns its framing state so it cannot be split
//! either. Putting either behind a mutex deadlocks the moment one thread parks
//! in a blocking read while holding it.
//!
//! So both sides are set non-blocking and pumped from one thread, which is the
//! same shape `netcmdr_session::Channel::poll` already uses. The cost is that an
//! idle connection has to poll rather than park; [`backoff`] keeps that cheap.
//!
//! # What crosses the wire
//!
//! Binary frames, each an opaque slice of the device's byte stream. The bridge
//! deliberately does **not** understand `KVMMsg` framing: message boundaries on
//! the WebSocket have nothing to do with message boundaries in the protocol,
//! and the client reassembles the stream exactly as it would from a socket.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use netcmdr_session::Stream;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::http::{self, Request};

/// RFC 6455's handshake constant.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// How much unsent browser-to-device data to accept before pausing reads.
///
/// Input messages are 28 bytes, so this is never reached in practice; it exists
/// so a device that stops reading cannot make us buffer without limit.
const TO_DEVICE_LIMIT: usize = 1024 * 1024;

/// Cap on WebSocket data buffered for a browser that is not keeping up.
///
/// Exceeding it is not fatal: `tungstenite` hands the message back, the pump
/// stops reading video until the buffer drains, and the device's own socket
/// buffer then applies the backpressure for us.
const WS_WRITE_LIMIT: usize = 8 * 1024 * 1024;

/// Device reads per iteration, so a busy video channel cannot starve the
/// keyboard direction of the same pump.
const READS_PER_TURN: usize = 16;

const READ_CHUNK: usize = 64 * 1024;

/// Complete a WebSocket upgrade that [`Request`] has already parsed.
///
/// Returns an error without writing anything if the request is not a valid
/// upgrade, leaving the caller to send the failure status it prefers.
pub fn accept(request: &Request, stream: &mut TcpStream) -> io::Result<WebSocket<TcpStream>> {
    if request.method != "GET" {
        return Err(http::bad("a WebSocket upgrade must be a GET"));
    }
    if !request.header_lists("upgrade", "websocket") {
        return Err(http::bad("not a websocket upgrade"));
    }
    if !request.header_lists("connection", "upgrade") {
        return Err(http::bad("upgrade requested without a Connection header"));
    }
    if request.header("sec-websocket-version") != Some("13") {
        return Err(http::bad("only WebSocket version 13 is supported"));
    }
    let key = request
        .header("sec-websocket-key")
        .ok_or_else(|| http::bad("no Sec-WebSocket-Key"))?;

    write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         \r\n",
        accept = accept_key(key),
    )?;
    stream.flush()?;

    let config = WebSocketConfig::default().max_write_buffer_size(WS_WRITE_LIMIT);
    Ok(WebSocket::from_partially_read(
        stream.try_clone()?,
        request.rest.clone(),
        Role::Server,
        Some(config),
    ))
}

/// `Sec-WebSocket-Accept`: base64(SHA-1(key ++ GUID)).
fn accept_key(key: &str) -> String {
    let digest = openssl::sha::sha1(format!("{key}{WS_GUID}").as_bytes());
    openssl::base64::encode_block(&digest)
}

/// Read the session id the client sends as its first message.
///
/// # Why this is not in the URL
///
/// The device's `SESSIONID` is base64 — measured on firmware 2.2.1258.1.0, a
/// 44-character token containing `+`, `/` and `=`, which are exactly the
/// characters a query string handles badly. That alone would only call for
/// careful encoding, but two other things settle it:
///
/// - It is a credential, and credentials in URLs end up in access logs, proxy
///   logs and browser history in a way message payloads do not.
/// - **A browser never exposes the body of a failed WebSocket handshake.** If
///   the device refuses, an HTTP status here would reach the page as nothing
///   more than "connection failed". Upgrading first and then refusing with a
///   close frame is the only way the reason reaches the client at all.
///
/// So the upgrade always succeeds, the first frame carries the token as text,
/// and everything after it is opaque protocol data.
pub fn read_session_id(ws: &mut WebSocket<TcpStream>) -> io::Result<String> {
    loop {
        return match ws.read() {
            Ok(Message::Text(text)) => Ok(text.as_str().to_string()),
            Ok(Message::Binary(_)) => Err(http::bad(
                "the first message must be the session id, as text",
            )),
            Ok(Message::Close(_)) => Err(http::bad("closed before sending a session id")),
            // A browser may ping before it says anything.
            Ok(_) => continue,
            Err(e) => Err(io::Error::other(e)),
        };
    }
}

/// Turn a client away with a reason it can actually read.
pub fn refuse(ws: &mut WebSocket<TcpStream>, code: CloseCode, reason: &str) {
    let _ = ws.close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }));
    let _ = ws.flush();
}

/// Move bytes between the browser and the device until either side stops.
///
/// Returns when the relay ends normally; a protocol or I/O failure on either
/// side comes back as an error.
pub fn relay(mut ws: WebSocket<TcpStream>, mut device: Stream) -> io::Result<()> {
    ws.get_ref().set_nonblocking(true)?;
    ws.get_ref().set_nodelay(true).ok();
    device.set_nonblocking(true)?;
    // Non-blocking supersedes the connect-time timeouts; leaving them set makes
    // some platforms report timeouts instead of `WouldBlock`.
    device.set_read_timeout(None)?;

    let mut to_device: Vec<u8> = Vec::new();
    let mut sent = 0usize;
    let mut pending: Option<Message> = None;
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut idle = 0u32;

    loop {
        let mut moved = false;

        // Browser -> us.
        while to_device.len() - sent < TO_DEVICE_LIMIT {
            match ws.read() {
                Ok(Message::Binary(data)) => {
                    to_device.extend_from_slice(&data);
                    moved = true;
                }
                Ok(Message::Close(_)) => {
                    tracing::debug!("browser closed the channel");
                    return Ok(());
                }
                // `tungstenite` answers pings itself; the reply leaves with the
                // next flush.
                Ok(Message::Ping(_) | Message::Pong(_)) => moved = true,
                Ok(other) => {
                    tracing::warn!(?other, "ignoring a non-binary frame");
                    moved = true;
                }
                Err(WsError::Io(e)) if would_block(&e) => break,
                Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(io::Error::other(e)),
            }
        }

        // Us -> device.
        while sent < to_device.len() {
            match device.write(&to_device[sent..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => {
                    sent += n;
                    moved = true;
                }
                Err(e) if would_block(&e) => break,
                Err(e) => return Err(e),
            }
        }
        if sent == to_device.len() {
            to_device.clear();
            sent = 0;
        }

        // A message the write buffer had no room for goes before any new one.
        if let Some(message) = pending.take() {
            match ws.write(message) {
                Ok(()) => moved = true,
                Err(WsError::WriteBufferFull(message)) => pending = Some(*message),
                Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(io::Error::other(e)),
            }
        }

        // Device -> browser, only while the browser is keeping up.
        let mut device_eof = false;
        if pending.is_none() {
            for _ in 0..READS_PER_TURN {
                match device.read(&mut chunk) {
                    Ok(0) => {
                        device_eof = true;
                        break;
                    }
                    Ok(n) => {
                        moved = true;
                        let message = Message::Binary(chunk[..n].to_vec().into());
                        match ws.write(message) {
                            Ok(()) => {}
                            Err(WsError::WriteBufferFull(message)) => {
                                pending = Some(*message);
                                break;
                            }
                            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => {
                                return Ok(())
                            }
                            Err(e) => return Err(io::Error::other(e)),
                        }
                    }
                    Err(e) if would_block(&e) => break,
                    Err(e) => return Err(e),
                }
            }
        }

        match ws.flush() {
            Ok(()) => {}
            Err(WsError::Io(e)) if would_block(&e) => {}
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(io::Error::other(e)),
        }

        if device_eof {
            tracing::debug!("device closed the channel");
            // Best effort: the browser may already be gone.
            let _ = ws.close(None);
            let _ = ws.flush();
            return Ok(());
        }

        if moved {
            idle = 0;
        } else {
            idle = idle.saturating_add(1);
            std::thread::sleep(backoff(idle));
        }
    }
}

/// How long to sleep after `idle` turns with nothing to do.
///
/// Video keeps a live session in the first band, so the latency that matters —
/// a keystroke while the screen is busy — is never paid. The later bands are
/// for a session sitting on a blank screensaver, where 8 ms before the first
/// byte moves is imperceptible and 125 wakeups a second is close to free.
fn backoff(idle: u32) -> Duration {
    match idle {
        0..=64 => Duration::from_micros(250),
        65..=256 => Duration::from_millis(2),
        _ => Duration::from_millis(8),
    }
}

fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// RFC 6455 §1.3 works the example through by hand.
    #[test]
    fn computes_the_handshake_key_from_the_rfc() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn rejects_a_request_that_is_not_an_upgrade() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let request = Request::read(&mut io::Cursor::new(
            b"GET /kvm HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        ))
        .unwrap()
        .unwrap();
        assert!(accept(&request, &mut client).is_err());
    }

    /// A connected pair of `tungstenite` endpoints and a fake device, wired
    /// through the real pump.
    fn wire() -> (WebSocket<TcpStream>, TcpStream, std::thread::JoinHandle<()>) {
        let browser_side = TcpListener::bind("127.0.0.1:0").unwrap();
        let browser_addr = browser_side.local_addr().unwrap();
        let device_side = TcpListener::bind("127.0.0.1:0").unwrap();
        let device_addr = device_side.local_addr().unwrap();

        let browser = TcpStream::connect(browser_addr).unwrap();
        let (server_sock, _) = browser_side.accept().unwrap();

        let relay_device = TcpStream::connect(device_addr).unwrap();
        let (device, _) = device_side.accept().unwrap();

        let server = WebSocket::from_raw_socket(server_sock, Role::Server, None);
        let pump = std::thread::spawn(move || {
            let _ = relay(server, Stream::Plain(relay_device));
        });

        // The browser end masks its frames, exactly as a real one does.
        let client = WebSocket::from_raw_socket(browser, Role::Client, None);
        (client, device, pump)
    }

    #[test]
    fn carries_bytes_from_the_browser_to_the_device() {
        let (mut client, mut device, pump) = wire();
        client
            .send(Message::Binary(b"hello device".to_vec().into()))
            .unwrap();

        let mut got = [0u8; 12];
        device.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello device");

        drop(client);
        drop(device);
        let _ = pump.join();
    }

    #[test]
    fn carries_bytes_from_the_device_to_the_browser() {
        let (mut client, mut device, pump) = wire();
        device.write_all(b"hello browser").unwrap();
        device.flush().unwrap();

        // The pump may split or coalesce; reassemble until we have it all.
        let mut got = Vec::new();
        while got.len() < 13 {
            match client.read().unwrap() {
                Message::Binary(b) => got.extend_from_slice(&b),
                Message::Close(_) => break,
                _ => {}
            }
        }
        assert_eq!(&got, b"hello browser");

        drop(device);
        drop(client);
        let _ = pump.join();
    }

    /// The device hanging up has to reach the browser as a close, not a hang.
    #[test]
    fn closes_the_browser_side_when_the_device_goes_away() {
        let (mut client, device, pump) = wire();
        drop(device);

        let closed = loop {
            match client.read() {
                Ok(Message::Close(_)) => break true,
                Ok(_) => continue,
                Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => break true,
                Err(_) => break false,
            }
        };
        assert!(closed, "the browser should have been told the device left");
        let _ = pump.join();
    }
}
