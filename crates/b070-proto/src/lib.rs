// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wire protocol types for Tripp Lite NetCommander / Minicom Smart IP KVM switches.
//!
//! The [`kvmmsg`] module is **authoritative**: it is transcribed from the
//! current firmware's own pure-Java client, pulled live from a device
//! (firmware 2.2.1258.1.0) on 2026-09-19. See `research/notes/FINDINGS.md` §5.4.
//!
//! The session runs four TCP connections (command, keyboard/mouse, video,
//! virtual media — see [`kvmmsg::SocketType`]); each opens with a short
//! plaintext handshake, two of them then upgrade to TLS, and all of them frame
//! their traffic with the same [`kvmmsg::Header`].
//!
//! [`command`] builds the app-level `<commands>` control documents. On the
//! current firmware these are the *facade* language the GUI hands to its plugin,
//! which re-encodes them as [`kvmmsg`] messages on the wire; they are not
//! themselves the wire format.

pub mod command;
pub mod kvmmsg;

/// Default TCP port for the KVM session channels.
///
/// Configurable on the device. The live device reports `ip_port="900"` and its
/// login reply advertises `port="900"`. Only 900 was found listening; 901/902
/// (seen in older firmware config) were closed.
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
}
