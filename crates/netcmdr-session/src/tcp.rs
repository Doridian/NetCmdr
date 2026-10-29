// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native transport: TCP, the device handshake, and optional TLS.
//!
//! Transcribed from `com.minicom.plugin.socket.{KVMSocket,SocketUtils}`.
//!
//! A session is four TCP connections to one port, one per [`SocketType`]:
//!
//! 1. write the session id and `#` (ASCII),
//! 2. write the socket type as a 4-byte LE integer,
//! 3. read back and check the 4-byte echo,
//! 4. start TLS for command and keyboard/mouse only.
//!
//! **Video and virtual media are unencrypted** by the device's design.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use netcmdr_proto::kvmmsg::SocketType;
use netcmdr_proto::tls::{self, TlsOptions};
use openssl::ssl::SslStream;

use crate::channel::{Channel, Transport};
use crate::Error;

/// A handshaken session socket: TLS for command and keyboard/mouse, plain
/// otherwise. Public so a byte relay can use [`connect_stream`] directly.
pub enum Stream {
    Plain(TcpStream),
    Tls(Box<SslStream<TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }

    /// `None` for blocking reads.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.tcp().set_read_timeout(timeout)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.tcp().set_nonblocking(nonblocking)
    }

    /// Unblocks a reader.
    pub fn shutdown(&self) {
        let _ = self.tcp().shutdown(std::net::Shutdown::Both);
    }

    pub fn shutdown_handle(&self) -> io::Result<ShutdownHandle> {
        Ok(ShutdownHandle(self.tcp().try_clone()?))
    }
}

impl Transport for Stream {
    fn close(&self) {
        self.shutdown();
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Shuts a connection down from another thread, waking a blocked reader.
pub struct ShutdownHandle(TcpStream);

impl ShutdownHandle {
    pub fn shutdown(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// Open a session socket and do the handshake (see module docs).
/// `session_id` is the `SESSIONID` from the HTTPS login.
pub fn connect_stream(
    host: &str,
    port: u16,
    session_id: &str,
    socket_type: SocketType,
    timeout: Duration,
    tls: &TlsOptions,
) -> Result<Stream, Error> {
    let tcp = TcpStream::connect((host, port))?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;

    let mut plain = tcp;
    plain.write_all(format!("{session_id}#").as_bytes())?;
    let code = socket_type.code();
    plain.write_all(&code.to_le_bytes())?;
    plain.flush()?;

    let mut echo = [0u8; 4];
    plain.read_exact(&mut echo)?;
    let echoed = i32::from_le_bytes(echo);
    if echoed != code {
        return Err(Error::Handshake {
            expected: code,
            got: echoed,
        });
    }

    let stream = if socket_type.uses_tls() {
        Stream::Tls(Box::new(
            tls::connect(tls, tls::SESSION_CIPHERS, host, plain).map_err(Error::Tls)?,
        ))
    } else {
        Stream::Plain(plain)
    };

    tracing::debug!(?socket_type, tls = socket_type.uses_tls(), "channel open");
    Ok(stream)
}

/// `KVMMsg` framing over a TCP session socket.
pub type TcpChannel = Channel<Stream>;

impl Channel<Stream> {
    pub fn connect(
        host: &str,
        port: u16,
        session_id: &str,
        socket_type: SocketType,
        timeout: Duration,
        tls: &TlsOptions,
    ) -> Result<Self, Error> {
        let stream = connect_stream(host, port, session_id, socket_type, timeout, tls)?;
        Ok(Channel::new(stream, socket_type))
    }

    /// `None` for blocking reads.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), Error> {
        self.transport().set_read_timeout(timeout)?;
        Ok(())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> Result<(), Error> {
        self.transport().set_nonblocking(nonblocking)?;
        Ok(())
    }

    /// Unblocks a reader.
    pub fn shutdown(&self) {
        self.transport().shutdown();
    }

    pub fn shutdown_handle(&self) -> Result<ShutdownHandle, Error> {
        Ok(self.transport().shutdown_handle()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;
    use netcmdr_proto::kvmmsg::{Cmd, CmdFlag, Header, Message};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Fake device: handshake, then send `script`.
    fn fake_device(script: Vec<u8>, expect_type: SocketType) -> (String, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // "<session>#" then the 4-byte type code.
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).unwrap();
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(text.starts_with("sess#"), "preamble was {text:?}");
            let code = if n >= 9 {
                i32::from_le_bytes(buf[5..9].try_into().unwrap())
            } else {
                let mut c = [0u8; 4];
                sock.read_exact(&mut c).unwrap();
                i32::from_le_bytes(c)
            };
            assert_eq!(code, expect_type.code());
            sock.write_all(&code.to_le_bytes()).unwrap();
            sock.write_all(&script).unwrap();
            // Keep open so EOF comes only on drop.
            std::thread::sleep(Duration::from_millis(200));
        });
        (addr.ip().to_string(), addr.port())
    }

    #[test]
    fn handshake_and_framing_round_trip() {
        let msg = Message::new(CmdFlag::Bmp, Cmd::Undefined, vec![1, 2, 3, 4]);
        let (host, port) = fake_device(msg.to_bytes(), SocketType::Video);

        let mut ch = Channel::connect(
            &host,
            port,
            "sess",
            SocketType::Video,
            Duration::from_secs(5),
            &TlsOptions::default(),
        )
        .unwrap();
        assert_eq!(ch.recv().unwrap(), msg);
    }

    #[test]
    fn a_mismatched_echo_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(&99i32.to_le_bytes()).unwrap();
        });
        let result = Channel::connect(
            &addr.ip().to_string(),
            addr.port(),
            "sess",
            SocketType::Video,
            Duration::from_secs(5),
            &TlsOptions::default(),
        );
        assert!(matches!(result, Err(Error::Handshake { got: 99, .. })));
    }

    /// A message split across two reads must resume where it left off.
    #[test]
    fn a_partial_message_resumes() {
        let msg = Message::new(CmdFlag::Bmp, Cmd::Undefined, vec![9; 100]);
        let bytes = msg.to_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(&SocketType::Video.code().to_le_bytes())
                .unwrap();
            sock.write_all(&bytes[..20]).unwrap();
            sock.flush().unwrap();
            std::thread::sleep(Duration::from_millis(150));
            sock.write_all(&bytes[20..]).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });

        let mut ch = Channel::connect(
            &addr.ip().to_string(),
            addr.port(),
            "sess",
            SocketType::Video,
            Duration::from_millis(30),
            &TlsOptions::default(),
        )
        .unwrap();
        // Early polls time out.
        let mut timed_out = false;
        let got = loop {
            match ch.poll().unwrap() {
                Some(m) => break m,
                None => timed_out = true,
            }
        };
        assert!(
            timed_out,
            "the split write should have made a poll time out"
        );
        assert_eq!(got, msg);
    }

    #[test]
    fn an_oversized_header_is_rejected() {
        let mut header = Header::new(CmdFlag::Bmp, Cmd::Undefined, 0)
            .encode()
            .to_vec();
        header[4..8].copy_from_slice(&(crate::channel::MAX_PAYLOAD as u32 + 1).to_le_bytes());
        let (host, port) = fake_device(header, SocketType::Video);
        let mut ch = Channel::connect(
            &host,
            port,
            "sess",
            SocketType::Video,
            Duration::from_secs(5),
            &TlsOptions::default(),
        )
        .unwrap();
        assert!(matches!(ch.recv(), Err(Error::OversizedMessage(_))));
    }
}
