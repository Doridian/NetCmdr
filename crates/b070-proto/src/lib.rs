//! Wire protocol types for Tripp Lite NetCommander / Minicom Smart IP KVM switches.
//!
//! Everything in this crate was derived by reverse engineering the vendor's
//! Java client and its native library `libkvmipjproxy.so`. **None of it has
//! been validated against real hardware yet.** Items the disassembly does not
//! prove are marked `[ASSUMED]` in the doc comments; see
//! `research/notes/FINDINGS.md` for the full derivation and
//! `research/notes/analysis/protocol-framing.disasm.txt` for the raw listings.
//!
//! Two framings ride inside the TLS session on TCP 900:
//!
//! - [`itcmsg`] — the KVM session message (video, input, control).
//! - [`commio`] — virtual media, which is USB-over-IP with a SCSI handler.
//!
//! [`command`] builds the XML control documents that the session carries.

pub mod command;
pub mod commio;
pub mod itcmsg;

/// Default TCP port for the KVM session channel.
///
/// Configurable on the device between 800 and 65535. The firmware's
/// `config.xml` lists `ip_port="900 , 901 , 902"`; the web UI's JavaScript
/// defaults to 902. Which of the three does what is not yet established.
pub const DEFAULT_SESSION_PORT: u16 = 900;

/// Default HTTPS port for the management API.
pub const DEFAULT_HTTPS_PORT: u16 = 443;

/// Default static IPv4 address when no DHCP server answers.
pub const DEFAULT_STATIC_IP: &str = "192.168.0.254";

/// Default TCP ports for direct SSH access to the two serial ports, when
/// enabled under Security settings.
pub const DEFAULT_SERIAL_SSH_PORTS: [u16; 2] = [4001, 4002];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("need {need} bytes, have {got}")]
    Truncated { need: usize, got: usize },

    #[error("bad ITCMsg magic {0:#010x}, expected 0x000056ce")]
    BadMagic(u32),

    #[error("unsupported ITCMsg version {0}, expected 2")]
    BadVersion(u16),

    #[error("unknown ITCMsg type {0}, expected 1..=4")]
    BadMsgType(u16),

    #[error("ITCMsg payload length is zero")]
    EmptyPayload,

    #[error("payload of {0} bytes exceeds the 16-bit CommIO length field")]
    PayloadTooLarge(usize),
}
