// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wire protocol types for Tripp Lite NetCommander / Minicom Smart IP KVM switches.
//!
//! [`kvmmsg`] is transcribed from the firmware's Java client (2.2.1258.1.0).
//! See `research/notes/FINDINGS.md` §5.4.
//!
//! A session uses four TCP connections ([`kvmmsg::SocketType`]), each with a
//! plaintext handshake; two upgrade to TLS. All use [`kvmmsg::Header`].
//!
//! [`command`] builds the app-level `<commands>` documents, which the client
//! re-encodes as [`kvmmsg`] messages.

pub mod command;
pub mod kvmmsg;

/// Default session port (configurable). 901/902 from older firmware were
/// closed on the live device.
pub const DEFAULT_SESSION_PORT: u16 = 900;

/// Default HTTPS port for the management API.
pub const DEFAULT_HTTPS_PORT: u16 = 443;

/// Default static IPv4 address when no DHCP server answers.
pub const DEFAULT_STATIC_IP: &str = "192.168.0.254";

/// Default SSH ports for the two serial ports.
pub const DEFAULT_SERIAL_SSH_PORTS: [u16; 2] = [4001, 4002];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("need {need} bytes, have {got}")]
    Truncated { need: usize, got: usize },
}
