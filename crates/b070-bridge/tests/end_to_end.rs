// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The whole bridge, end to end: an HTTP request arrives, becomes a WebSocket,
//! the device's session handshake completes behind it, and a real `KVMMsg`
//! survives the round trip in both directions.
//!
//! The fake device answers the handshake exactly as `b070_session::channel`'s
//! own tests describe it — session id, `#`, the socket-type code, and the code
//! echoed back. The video channel is used because it is the one the device
//! leaves in the clear, so the test needs no certificate; the TLS upgrade on
//! the command channel is the same `connect_stream` call either way.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use b070_bridge::Bridge;
use b070_proto::kvmmsg::{Cmd, CmdFlag, Header, Message, SocketType, HEADER_LEN};
use tungstenite::protocol::Role;
use tungstenite::{Message as WsMessage, WebSocket};

/// Stand in for the device: complete the session handshake, then run `script`.
fn fake_device(script: impl FnOnce(TcpStream) + Send + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(text.starts_with("testsession#"), "preamble was {text:?}");
        let code = if n >= 16 {
            i32::from_le_bytes(buf[12..16].try_into().unwrap())
        } else {
            let mut c = [0u8; 4];
            sock.read_exact(&mut c).unwrap();
            i32::from_le_bytes(c)
        };
        assert_eq!(code, SocketType::Video.code());
        sock.write_all(&code.to_le_bytes()).unwrap();
        script(sock);
    });
    port
}

/// Start the bridge on a loopback port, pointed at `session_port`.
fn start_bridge(session_port: u16) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bridge = Bridge {
        host: "127.0.0.1".into(),
        https_port: 443,
        session_port,
        assets: None,
        timeout: Duration::from_secs(5),
    };
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let bridge = bridge.clone();
            let mut stream = incoming.unwrap();
            std::thread::spawn(move || {
                let _ = b070_bridge::route(&bridge, &mut stream);
            });
        }
    });
    port
}

/// Do the upgrade by hand and hand the socket to `tungstenite` as a client.
///
/// The client role is what masks outgoing frames, so this exercises the
/// bridge's unmasking the same way a browser would.
fn connect_ws(bridge_port: u16, target: &str, session: &str) -> WebSocket<TcpStream> {
    let mut sock = TcpStream::connect(("127.0.0.1", bridge_port)).unwrap();
    write!(
        sock,
        "GET {target} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Upgrade: websocket\r\n\
         Connection: keep-alive, Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .unwrap();
    sock.flush().unwrap();

    // Read the response head one byte at a time so nothing past it is consumed.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(
            sock.read(&mut byte).unwrap(),
            1,
            "bridge closed mid-upgrade"
        );
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101 "),
        "upgrade was refused: {head}"
    );
    assert!(
        head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "wrong accept key: {head}"
    );

    let mut ws = WebSocket::from_raw_socket(sock, Role::Client, None);
    // The session id is the first message, never the URL.
    ws.send(WsMessage::Text(session.into())).unwrap();
    ws
}

/// Collect binary frames until `want` bytes have arrived.
fn read_bytes(ws: &mut WebSocket<TcpStream>, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        match ws.read().unwrap() {
            WsMessage::Binary(b) => got.extend_from_slice(&b),
            WsMessage::Close(_) => panic!("closed after {} of {want} bytes", got.len()),
            _ => {}
        }
    }
    got
}

#[test]
fn a_kvm_message_survives_the_round_trip_through_the_bridge() {
    let from_device = Message::new(CmdFlag::Bmp, Cmd::Undefined, vec![7; 300]);
    let to_device = Message::new(CmdFlag::RawData, Cmd::VerifyUser, b" admin ; tok".to_vec());

    let expected_upstream = to_device.to_bytes();
    let device_script = from_device.to_bytes();
    let upstream_len = expected_upstream.len();

    let (tx, rx) = std::sync::mpsc::channel();
    let device_port = fake_device(move |mut sock| {
        sock.write_all(&device_script).unwrap();
        sock.flush().unwrap();
        // Read back whatever the browser sent through the relay.
        let mut got = vec![0u8; upstream_len];
        sock.read_exact(&mut got).unwrap();
        tx.send(got).unwrap();
        // Hold the socket open so the relay is not torn down under the client.
        std::thread::sleep(Duration::from_millis(300));
    });

    let bridge_port = start_bridge(device_port);
    let mut ws = connect_ws(bridge_port, "/kvm?type=video", "testsession");

    // Device -> browser, with framing intact.
    let got = read_bytes(&mut ws, from_device.to_bytes().len());
    assert_eq!(got, from_device.to_bytes());
    let head: &[u8; HEADER_LEN] = got[..HEADER_LEN].try_into().unwrap();
    let header = Header::decode(head).unwrap();
    assert_eq!(header.cmd_flag, CmdFlag::Bmp);
    assert_eq!(header.size as usize, 300);

    // Browser -> device.
    ws.send(WsMessage::Binary(expected_upstream.clone().into()))
        .unwrap();
    let upstream = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(upstream, expected_upstream);
}

/// The device's `SESSIONID` is base64: measured on firmware 2.2.1258.1.0 it is
/// 44 characters and routinely contains `+`, `/` and `=`. Those are exactly the
/// characters a query string mangles, which is why the token travels as a
/// WebSocket message — this pins that it arrives byte for byte.
#[test]
fn a_base64_session_id_reaches_the_device_verbatim() {
    let id = "l7tnnoW74mnAbr41nIIUH+9KzFeUexlKTjC2Kt39VQk=";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let device_port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).unwrap();
        tx.send(String::from_utf8_lossy(&buf[..n]).into_owned())
            .unwrap();
        let _ = sock.write_all(&SocketType::Video.code().to_le_bytes());
        std::thread::sleep(Duration::from_millis(200));
    });

    let bridge_port = start_bridge(device_port);
    let _ws = connect_ws(bridge_port, "/kvm?type=video", id);
    let preamble = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        preamble.starts_with(&format!("{id}#")),
        "device saw {preamble:?}, not the id as sent"
    );
}

/// A browser is never shown the body of a failed upgrade, so a device that
/// refuses has to come back as a close frame carrying the reason.
#[test]
fn a_device_refusal_arrives_as_a_close_frame_with_a_reason() {
    let bridge_port = start_bridge(1); // nothing is listening on port 1
    let mut ws = connect_ws(bridge_port, "/kvm?type=video", "whatever");

    let reason = loop {
        match ws.read() {
            Ok(WsMessage::Close(Some(frame))) => break frame.reason.to_string(),
            Ok(_) => continue,
            Err(e) => panic!("expected a close frame, got {e}"),
        }
    };
    assert!(
        reason.starts_with("device: "),
        "the close reason should name the device: {reason:?}"
    );
}

/// An unknown channel name must be refused outright, not guessed at.
#[test]
fn an_unknown_channel_type_is_rejected_before_any_device_connection() {
    let bridge_port = start_bridge(1); // nothing listens there; nothing should try
    let mut sock = TcpStream::connect(("127.0.0.1", bridge_port)).unwrap();
    write!(
        sock,
        "GET /kvm?type=nonsense&session=x HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .unwrap();
    let mut reply = String::new();
    sock.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 400 "), "got {reply}");
}

/// Without `--static-dir` the bridge still answers the protocol routes, and
/// says a clear no to everything else.
#[test]
fn serves_its_own_description_and_nothing_else() {
    let bridge_port = start_bridge(1);

    let mut sock = TcpStream::connect(("127.0.0.1", bridge_port)).unwrap();
    write!(sock, "GET /api/info HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut reply = String::new();
    sock.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200 "), "got {reply}");
    assert!(reply.contains(r#""device":"127.0.0.1""#), "got {reply}");

    let mut sock = TcpStream::connect(("127.0.0.1", bridge_port)).unwrap();
    write!(sock, "GET /index.html HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut reply = String::new();
    sock.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 404 "), "got {reply}");
}
