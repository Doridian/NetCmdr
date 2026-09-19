//! One session socket: the plaintext handshake, the optional TLS upgrade, and
//! `KVMMsg` framing on top.
//!
//! Transcribed from `com.minicom.plugin.socket.{KVMSocket,SocketUtils}`.
//!
//! A session is **four separate TCP connections** to the same port, one per
//! [`SocketType`]. Each opens the same way:
//!
//! 1. write the management session id followed by `#`, as ASCII,
//! 2. write the socket-type code as a 4-byte little-endian integer,
//! 3. read 4 bytes back and check the device echoed the same code,
//! 4. for the command and keyboard/mouse sockets only, start TLS.
//!
//! **Video and virtual media stay in the clear** — that is the device's design,
//! not a shortcut taken here.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use b070_proto::kvmmsg::{Header, Message, SocketType, HEADER_LEN};
use openssl::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode, SslVersion};

use crate::Error;

/// Refuse to buffer a payload larger than this; a plausible frame is well under
/// it, and a corrupt header must not turn into a huge allocation.
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

enum Stream {
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

/// Partially received message, so a read that times out mid-frame can be
/// resumed rather than losing its place in the stream.
#[derive(Default)]
struct Incoming {
    header_buf: [u8; HEADER_LEN],
    header_got: usize,
    header: Option<Header>,
    payload: Vec<u8>,
    payload_got: usize,
}

/// One connected session channel.
pub struct Channel {
    stream: Stream,
    socket_type: SocketType,
    incoming: Incoming,
}

impl Channel {
    /// Open a channel and complete its handshake.
    ///
    /// `session_id` is the `SESSIONID` from the HTTPS login — the same token the
    /// management API returns, which is also what the command channel sends as
    /// its "password" when verifying the user.
    pub fn connect(
        host: &str,
        port: u16,
        session_id: &str,
        socket_type: SocketType,
        timeout: Duration,
    ) -> Result<Self, Error> {
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
            Stream::Tls(Box::new(start_tls(host, plain)?))
        } else {
            Stream::Plain(plain)
        };

        tracing::debug!(?socket_type, tls = socket_type.uses_tls(), "channel open");
        Ok(Channel {
            stream,
            socket_type,
            incoming: Incoming::default(),
        })
    }

    pub fn socket_type(&self) -> SocketType {
        self.socket_type
    }

    /// Set the read timeout, or clear it for blocking reads.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), Error> {
        self.stream.tcp().set_read_timeout(timeout)?;
        Ok(())
    }

    /// Send one message (header and payload in a single write, as the client
    /// does — the device's reader expects them together).
    pub fn send(&mut self, msg: &Message) -> Result<(), Error> {
        self.stream.write_all(&msg.to_bytes())?;
        self.stream.flush()?;
        Ok(())
    }

    /// Read one message, blocking until it is complete.
    pub fn recv(&mut self) -> Result<Message, Error> {
        loop {
            if let Some(msg) = self.poll()? {
                return Ok(msg);
            }
        }
    }

    /// Read whatever is available. Returns `Ok(None)` if the current message is
    /// still incomplete when the read timeout expires; the partial state is
    /// kept, so calling again resumes it.
    pub fn poll(&mut self) -> Result<Option<Message>, Error> {
        if self.incoming.header.is_none() {
            let got = self.incoming.header_got;
            match self.stream.read(&mut self.incoming.header_buf[got..]) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.incoming.header_got += n,
                Err(e) if would_block(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            if self.incoming.header_got < HEADER_LEN {
                return Ok(None);
            }
            let header = Header::decode(&self.incoming.header_buf)?;
            let size = header.size as usize;
            if size > MAX_PAYLOAD {
                return Err(Error::OversizedMessage(size));
            }
            self.incoming.payload = vec![0; size];
            self.incoming.payload_got = 0;
            self.incoming.header = Some(header);
        }

        while self.incoming.payload_got < self.incoming.payload.len() {
            let got = self.incoming.payload_got;
            match self.stream.read(&mut self.incoming.payload[got..]) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.incoming.payload_got += n,
                Err(e) if would_block(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }

        let header = self.incoming.header.take().expect("header present");
        let payload = std::mem::take(&mut self.incoming.payload);
        self.incoming.header_got = 0;
        self.incoming.payload_got = 0;
        Ok(Some(Message { header, payload }))
    }

    /// Close the underlying socket, unblocking a reader.
    pub fn shutdown(&self) {
        let _ = self.stream.tcp().shutdown(std::net::Shutdown::Both);
    }

    /// A handle that can shut this channel down from another thread.
    pub fn shutdown_handle(&self) -> Result<ShutdownHandle, Error> {
        Ok(ShutdownHandle(self.stream.tcp().try_clone()?))
    }
}

/// Lets another thread drop a channel's connection, which is how a blocked
/// reader is woken for shutdown.
pub struct ShutdownHandle(TcpStream);

impl ShutdownHandle {
    pub fn shutdown(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Start TLS on an already-handshaken socket.
///
/// The session sockets negotiate TLS 1.2 with `AES256-GCM-SHA384`. The only
/// concession the device still needs is security level 1, because the DHE group
/// it offers on port 443 is 1024-bit and one connector is used for both.
///
/// The certificate is self-signed, so there is no trust anchor to check it
/// against. The vendor's own client installs a trust-all `X509TrustManager`
/// here, so this is no weaker than the stock client — but it does mean the
/// session is only as private as the network it crosses.
fn start_tls(host: &str, tcp: TcpStream) -> Result<SslStream<TcpStream>, Error> {
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_cipher_list("DEFAULT@SECLEVEL=1")?;
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    let mut cfg = connector.configure()?;
    cfg.set_verify_hostname(false);
    cfg.set_use_server_name_indication(false);
    cfg.connect(host, tcp)
        .map_err(|e| Error::Tls(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use b070_proto::kvmmsg::{Cmd, CmdFlag};
    use std::net::TcpListener;

    /// Stand in for the device: complete the handshake, then play a script of
    /// bytes back at the client.
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
            // Hold the socket open so the client sees EOF only when we drop it.
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
        )
        .unwrap();
        // Poll until the whole message arrives; the early polls time out.
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
        header[4..8].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        let (host, port) = fake_device(header, SocketType::Video);
        let mut ch = Channel::connect(
            &host,
            port,
            "sess",
            SocketType::Video,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(ch.recv(), Err(Error::OversizedMessage(_))));
    }
}
