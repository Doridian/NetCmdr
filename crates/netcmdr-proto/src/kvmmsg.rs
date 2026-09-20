// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `KVMMsg` — the unified KVM session message.
//!
//! **Authoritative.** The layout, enums and encoding here are transcribed from
//! the current firmware's own pure-Java client (firmware 2.2.1258.1.0, pulled live
//! from a B070-008-19-IP on 2026-09-18), namely
//! `com.minicom.plugin.serialization.KVMMsgHeaderEncoder` / `KVMMSgHeaderDecoder`
//! and `com.minicom.plugin.data.{KVMMsgHeader,MagicNumber,CmdFlag,Cmd,SocketType}`.
//!
//! This supersedes the earlier `ITCMsg`/`CommIO` framing that was reverse
//! engineered from the 2013 native library `libkvmipjproxy.so`: the 2017
//! firmware reimplemented the whole protocol in Java and unified every channel
//! (command, keyboard/mouse, video, virtual media) onto this one message.
//!
//! ## Header (32 bytes, native byte order = little-endian on the x86 client)
//!
//! | Off | Size | Field | Notes |
//! |----:|-----:|-------|-------|
//! | 0  | u32 | magic | [`MagicNumber`]; `ALX_TAG` = 22222 for normal messages |
//! | 4  | u32 | size | payload length (heartbeat writes 0 here) |
//! | 8  | u32 | server_timestamp | |
//! | 12 | u32 | client_timestamp | |
//! | 16 | u16 | version | always 2 |
//! | 18 | u16 | cmd_flag | [`CmdFlag`] |
//! | 20 | u32 | command | [`Cmd`] |
//! | 24 | u32 | id | |
//! | 28 | u8  | key_frame | 1 = video keyframe |
//! | 29 | 3×u8 | padding | zero |
//!
//! The heartbeat variant (magic `ALX_HEARTBEAT`) reuses the timestamp fields and
//! zeroes size/cmd_flag/command; see [`Header::encode`].

use crate::Error;

/// Fixed header length.
pub const HEADER_LEN: usize = 32;

/// Protocol version. `KVMMsgFactory` always sets 2, and the reader rejects
/// anything else on non-heartbeat messages.
pub const VERSION: u16 = 2;

/// The value in the first header word. From `MagicNumber`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagicNumber {
    /// Ordinary message. 22222 == `0x56CE`.
    Tag,
    /// Round-trip heartbeat. 77777.
    Heartbeat,
    /// Session teardown. 88888.
    Bye,
    /// Anything unrecognised (decoder maps unknown codes here).
    Unknown(i32),
}

impl MagicNumber {
    pub fn code(self) -> i32 {
        match self {
            MagicNumber::Tag => 22222,
            MagicNumber::Heartbeat => 77777,
            MagicNumber::Bye => 88888,
            MagicNumber::Unknown(c) => c,
        }
    }

    pub fn from_code(c: i32) -> Self {
        match c {
            22222 => MagicNumber::Tag,
            77777 => MagicNumber::Heartbeat,
            88888 => MagicNumber::Bye,
            other => MagicNumber::Unknown(other),
        }
    }
}

/// Payload classification (`CmdFlag`), header offset 18.
///
/// Unlike the earlier RE guess (which thought this field was limited to 1–4),
/// it spans 0–21 and selects how the payload is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdFlag {
    KeyFrame,
    Palette,
    RawData,
    Bmp,
    General,
    UserLevel,
    CaptureStat,
    NoSignal,
    NamePwLevel,
    ReportProblem,
    MouseAcc,
    GuiMsg,
    ResumeConnect,
    UserLog,
    AutoAdjustProgress,
    /// Payload is an XML document.
    Xml,
    Disconnect,
    /// Any other value seen on the wire.
    Other(u16),
}

impl CmdFlag {
    pub fn code(self) -> u16 {
        match self {
            CmdFlag::KeyFrame => 0,
            CmdFlag::Palette => 1,
            CmdFlag::RawData => 2,
            CmdFlag::Bmp => 3,
            CmdFlag::General => 4,
            CmdFlag::UserLevel => 5,
            CmdFlag::CaptureStat => 8,
            CmdFlag::NoSignal => 9,
            CmdFlag::NamePwLevel => 13,
            CmdFlag::ReportProblem => 14,
            CmdFlag::MouseAcc => 15,
            CmdFlag::GuiMsg => 16,
            CmdFlag::ResumeConnect => 17,
            CmdFlag::UserLog => 18,
            CmdFlag::AutoAdjustProgress => 19,
            CmdFlag::Xml => 20,
            CmdFlag::Disconnect => 21,
            CmdFlag::Other(v) => v,
        }
    }

    pub fn from_code(v: u16) -> Self {
        match v {
            0 => CmdFlag::KeyFrame,
            1 => CmdFlag::Palette,
            2 => CmdFlag::RawData,
            3 => CmdFlag::Bmp,
            4 => CmdFlag::General,
            5 => CmdFlag::UserLevel,
            8 => CmdFlag::CaptureStat,
            9 => CmdFlag::NoSignal,
            13 => CmdFlag::NamePwLevel,
            14 => CmdFlag::ReportProblem,
            15 => CmdFlag::MouseAcc,
            16 => CmdFlag::GuiMsg,
            17 => CmdFlag::ResumeConnect,
            18 => CmdFlag::UserLog,
            19 => CmdFlag::AutoAdjustProgress,
            20 => CmdFlag::Xml,
            21 => CmdFlag::Disconnect,
            other => CmdFlag::Other(other),
        }
    }
}

/// The socket a session channel opens with, from `SocketType`.
///
/// The client makes four connections. `Cmd` and `KeyboardMouse` upgrade to TLS;
/// `Video` and `VirtualMedia` run in the clear. Every socket shares this header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    General,
    Cmd,
    KeyboardMouse,
    Video,
    VirtualMedia,
}

impl SocketType {
    pub fn code(self) -> i32 {
        match self {
            SocketType::General => -1,
            SocketType::Cmd => 0,
            SocketType::KeyboardMouse => 1,
            SocketType::Video => 2,
            SocketType::VirtualMedia => 3,
        }
    }

    /// Whether this channel upgrades to TLS after the plaintext handshake.
    pub fn uses_tls(self) -> bool {
        matches!(self, SocketType::Cmd | SocketType::KeyboardMouse)
    }
}

/// The `command` field (offset 20), from `Cmd`. Only the values the client
/// actively uses are named; everything else round-trips as [`Cmd::Other`].
///
/// The full enumeration (90+ values) is catalogued in
/// `research/notes/FINDINGS.md` §5.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Undefined,
    VerifyUser,
    Connect,
    Disconnect,
    Initialize,
    ChangeServer,
    GoLocal,
    RefreshScreen,
    VideoAuto,
    VideoStopAdjust,
    ConfigXml,
    MsgCmdXml,
    ClientUsbMedia,
    ClientMountUsbMedia,
    TerminateUsbMedia,
    OpenTtySession,
    CloseTtySession,
    TtyData,
    MouseMove,
    Other(i32),
}

impl Cmd {
    pub fn code(self) -> i32 {
        match self {
            Cmd::Undefined => 0,
            Cmd::VerifyUser => 5,
            Cmd::Connect => 14,
            Cmd::Disconnect => 15,
            Cmd::Initialize => 16,
            Cmd::ChangeServer => 19,
            Cmd::GoLocal => 40,
            Cmd::RefreshScreen => 53,
            Cmd::VideoAuto => 64,
            Cmd::VideoStopAdjust => 65,
            Cmd::ConfigXml => 71,
            Cmd::MsgCmdXml => 77,
            Cmd::ClientUsbMedia => 72,
            Cmd::ClientMountUsbMedia => 73,
            Cmd::TerminateUsbMedia => 74,
            Cmd::OpenTtySession => 56,
            Cmd::CloseTtySession => 57,
            Cmd::TtyData => 58,
            Cmd::MouseMove => 76,
            Cmd::Other(c) => c,
        }
    }

    pub fn from_code(c: i32) -> Self {
        match c {
            0 => Cmd::Undefined,
            5 => Cmd::VerifyUser,
            14 => Cmd::Connect,
            15 => Cmd::Disconnect,
            16 => Cmd::Initialize,
            19 => Cmd::ChangeServer,
            40 => Cmd::GoLocal,
            53 => Cmd::RefreshScreen,
            64 => Cmd::VideoAuto,
            65 => Cmd::VideoStopAdjust,
            71 => Cmd::ConfigXml,
            77 => Cmd::MsgCmdXml,
            72 => Cmd::ClientUsbMedia,
            73 => Cmd::ClientMountUsbMedia,
            74 => Cmd::TerminateUsbMedia,
            56 => Cmd::OpenTtySession,
            57 => Cmd::CloseTtySession,
            58 => Cmd::TtyData,
            76 => Cmd::MouseMove,
            other => Cmd::Other(other),
        }
    }
}

/// The 32-byte `KVMMsg` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub magic: MagicNumber,
    /// Payload length. Zero on a heartbeat.
    pub size: u32,
    pub server_timestamp: u32,
    pub client_timestamp: u32,
    pub version: u16,
    pub cmd_flag: CmdFlag,
    pub command: Cmd,
    pub id: u32,
    /// 1 marks a video keyframe.
    pub key_frame: u8,
}

impl Header {
    /// A normal (non-heartbeat) message header, as `KVMMsgFactory` builds it.
    pub fn new(cmd_flag: CmdFlag, command: Cmd, size: u32) -> Self {
        Header {
            magic: MagicNumber::Tag,
            size,
            server_timestamp: 0,
            client_timestamp: 0,
            version: VERSION,
            cmd_flag,
            command,
            id: 0,
            key_frame: 0,
        }
    }

    /// Encode exactly as `KVMMsgHeaderEncoder.encode` does, including the
    /// heartbeat special case.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&self.magic.code().to_le_bytes());
        if self.magic == MagicNumber::Heartbeat {
            // size stays 0; timestamps and id carried; cmd_flag/command zeroed.
            b[8..12].copy_from_slice(&self.server_timestamp.to_le_bytes());
            b[12..16].copy_from_slice(&self.client_timestamp.to_le_bytes());
            b[16..18].copy_from_slice(&self.version.to_le_bytes());
            // bytes 18..20 already zero
            b[24..28].copy_from_slice(&self.id.to_le_bytes());
            b[28] = self.key_frame;
        } else {
            b[4..8].copy_from_slice(&self.size.to_le_bytes());
            b[8..12].copy_from_slice(&self.server_timestamp.to_le_bytes());
            b[12..16].copy_from_slice(&self.client_timestamp.to_le_bytes());
            b[16..18].copy_from_slice(&self.version.to_le_bytes());
            b[18..20].copy_from_slice(&self.cmd_flag.code().to_le_bytes());
            b[20..24].copy_from_slice(&self.command.code().to_le_bytes());
            b[24..28].copy_from_slice(&self.id.to_le_bytes());
            b[28] = self.key_frame;
        }
        b
    }

    /// Decode a 32-byte header, mirroring `KVMMSgHeaderDecoder.decode`.
    ///
    /// Note the decoder does not resolve [`Cmd`] when `cmd_flag` is
    /// [`CmdFlag::MouseAcc`] (that field carries mouse data, not a command); we
    /// reproduce that by leaving `command` as [`Cmd::Undefined`] in that case.
    pub fn decode(buf: &[u8]) -> Result<Self, Error> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Truncated {
                need: HEADER_LEN,
                got: buf.len(),
            });
        }
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        let i32_at = |o: usize| i32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(buf[o..o + 2].try_into().unwrap());

        let magic = MagicNumber::from_code(i32_at(0));
        if magic == MagicNumber::Heartbeat {
            return Ok(Header {
                magic,
                size: 0,
                server_timestamp: u32_at(8),
                client_timestamp: u32_at(12),
                version: 0,
                cmd_flag: CmdFlag::Other(0),
                command: Cmd::Undefined,
                id: 0,
                key_frame: 0,
            });
        }

        let cmd_flag = CmdFlag::from_code(u16_at(18));
        let command = if cmd_flag == CmdFlag::MouseAcc {
            Cmd::Undefined
        } else {
            Cmd::from_code(i32_at(20))
        };
        Ok(Header {
            magic,
            size: u32_at(4),
            server_timestamp: u32_at(8),
            client_timestamp: u32_at(12),
            version: u16_at(16),
            cmd_flag,
            command,
            id: u32_at(24),
            key_frame: buf[28],
        })
    }
}

/// A complete message: header plus payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Message {
    /// Build a normal message (magic `ALX_TAG`), sizing the header to `payload`.
    pub fn new(cmd_flag: CmdFlag, command: Cmd, payload: Vec<u8>) -> Self {
        Message {
            header: Header::new(cmd_flag, command, payload.len() as u32),
            payload,
        }
    }

    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse one message from the front of `buf`, returning it and bytes used.
    ///
    /// Heartbeats carry no payload (`size` is 0 in the header).
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), Error> {
        let header = Header::decode(buf)?;
        let total = HEADER_LEN + header.size as usize;
        if buf.len() < total {
            return Err(Error::Truncated {
                need: total,
                got: buf.len(),
            });
        }
        Ok((
            Message {
                header,
                payload: buf[HEADER_LEN..total].to_vec(),
            },
            total,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_tag_is_0x56ce() {
        assert_eq!(MagicNumber::Tag.code(), 0x56CE);
        assert_eq!(MagicNumber::Tag.code(), 22222);
    }

    #[test]
    fn header_byte_layout_matches_java_encoder() {
        // Offsets asserted against KVMMsgHeaderEncoder.
        let h = Header {
            id: 0x11223344,
            server_timestamp: 0xAABBCCDD,
            key_frame: 1,
            ..Header::new(CmdFlag::Xml, Cmd::ConfigXml, 0x0102)
        };
        let b = h.encode();
        assert_eq!(&b[0..4], &22222i32.to_le_bytes(), "magic ALX_TAG @0");
        assert_eq!(&b[4..8], &[0x02, 0x01, 0, 0], "size @4");
        assert_eq!(&b[8..12], &0xAABBCCDDu32.to_le_bytes(), "server ts @8");
        assert_eq!(&b[16..18], &[2, 0], "version @16");
        assert_eq!(&b[18..20], &20u16.to_le_bytes(), "cmd_flag XML=20 @18");
        assert_eq!(
            &b[20..24],
            &71i32.to_le_bytes(),
            "command CONFIG_XML=71 @20"
        );
        assert_eq!(&b[24..28], &0x11223344u32.to_le_bytes(), "id @24");
        assert_eq!(b[28], 1, "key_frame @28");
        assert_eq!(&b[29..32], &[0, 0, 0], "padding @29");
    }

    #[test]
    fn header_roundtrips() {
        let h = Header {
            id: 42,
            client_timestamp: 999,
            ..Header::new(CmdFlag::RawData, Cmd::VerifyUser, 7)
        };
        assert_eq!(Header::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn heartbeat_variant_zeroes_size_and_command() {
        let h = Header {
            magic: MagicNumber::Heartbeat,
            server_timestamp: 5,
            client_timestamp: 6,
            ..Header::new(CmdFlag::General, Cmd::Connect, 1234)
        };
        let b = h.encode();
        assert_eq!(&b[0..4], &77777i32.to_le_bytes());
        assert_eq!(&b[4..8], &[0, 0, 0, 0], "heartbeat writes size 0");
        assert_eq!(&b[8..12], &5u32.to_le_bytes());
        let back = Header::decode(&b).unwrap();
        assert_eq!(back.magic, MagicNumber::Heartbeat);
        assert_eq!(back.server_timestamp, 5);
        assert_eq!(back.size, 0);
    }

    #[test]
    fn mouse_acc_flag_suppresses_command_decode() {
        // KVMMSgHeaderDecoder skips Cmd.valueOf when cmd_flag == MOUSE_ACC.
        let mut b = Header::new(CmdFlag::MouseAcc, Cmd::Connect, 4).encode();
        // put a bogus command code at offset 20; decode must ignore it
        b[20..24].copy_from_slice(&999i32.to_le_bytes());
        let h = Header::decode(&b).unwrap();
        assert_eq!(h.cmd_flag, CmdFlag::MouseAcc);
        assert_eq!(h.command, Cmd::Undefined);
    }

    #[test]
    fn message_stream_decode() {
        let m = Message::new(
            CmdFlag::RawData,
            Cmd::VerifyUser,
            b" admin ; pw ; 0".to_vec(),
        );
        let bytes = m.to_bytes();
        let (back, n) = Message::parse(&bytes).unwrap();
        assert_eq!(back, m);
        assert_eq!(n, bytes.len());
    }

    #[test]
    fn socket_tls_split_matches_client() {
        assert!(SocketType::Cmd.uses_tls());
        assert!(SocketType::KeyboardMouse.uses_tls());
        assert!(!SocketType::Video.uses_tls());
        assert!(!SocketType::VirtualMedia.uses_tls());
    }

    #[test]
    fn unknown_codes_round_trip() {
        assert_eq!(Cmd::from_code(9999), Cmd::Other(9999));
        assert_eq!(Cmd::Other(9999).code(), 9999);
        assert_eq!(CmdFlag::from_code(250), CmdFlag::Other(250));
    }
}
