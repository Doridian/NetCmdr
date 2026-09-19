//! KVM session client for Tripp Lite NetCommander / Minicom Smart IP switches:
//! live video, keyboard and mouse.
//!
//! Everything here is transcribed from the current firmware's own Java client
//! (firmware 2.2.1258.1.0, pulled live from a B070-008-19-IP), whose sources are
//! preserved under `research/decompiled/app-live/`. See
//! `research/notes/FINDINGS.md` §5.4 for the protocol write-up.
//!
//! ```no_run
//! use std::time::Duration;
//! use b070_session::{Session, SessionConfig, Event, Input};
//!
//! // Log in over HTTPS first (b070-mgmt) to get a SESSIONID, then:
//! let cfg = SessionConfig::new("192.0.2.10", 900, "SESSIONID", "admin", 1);
//! let session = Session::connect(cfg)?;
//!
//! session.send_input(Input::MouseMove { x: 100, y: 100 })?;
//! while let Ok(event) = session.events.recv() {
//!     match event {
//!         Event::Frame(rect) => {
//!             let frame = session.frame().lock().unwrap();
//!             // frame.pixels is width * height u32s, each 0x00RRGGBB
//!             let _ = (rect, frame.width);
//!         }
//!         Event::Disconnected(reason) => { eprintln!("{reason}"); break }
//!         _ => {}
//!     }
//! }
//! # Ok::<(), b070_session::Error>(())
//! ```
//!
//! Virtual media (the fourth channel) is not implemented.

pub mod channel;
pub mod input;
pub mod lzw;
pub mod session;
pub mod video;
pub mod ycbcr;

pub use channel::Channel;
pub use input::{Input, Modifiers, MouseButton};
pub use session::{Event, Session, SessionConfig};
pub use video::{Framebuffer, Rect, SharedFrame, VideoDecoder};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("OpenSSL error: {0}")]
    OpenSsl(#[from] openssl::error::ErrorStack),

    /// The device did not echo the socket-type code back during the handshake.
    #[error("handshake failed: sent socket type {expected}, device echoed {got}")]
    Handshake { expected: i32, got: i32 },

    #[error("connection closed")]
    Closed,

    #[error("message claims a {0} byte payload")]
    OversizedMessage(usize),

    #[error("protocol error: {0}")]
    Proto(#[from] b070_proto::Error),

    #[error("LZW stream error: {0}")]
    Lzw(&'static str),

    #[error("zlib error: {0}")]
    Inflate(String),

    #[error("video payload ended early")]
    ShortVideoPayload,

    #[error("bad video field: {0}")]
    BadVideoField(&'static str),

    /// A pixel format the vendor's own client does not decode either.
    #[error("unsupported pixel format {0}")]
    UnsupportedFormat(i16),

    #[error("unsupported compression method {0}")]
    UnsupportedCompression(i16),

    #[error("login refused: {0}")]
    LoginRefused(&'static str),

    #[error("device did not answer the login")]
    LoginTimeout,
}
