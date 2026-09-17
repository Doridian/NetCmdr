//! `ITCMsg` — the KVM session message that rides inside the TLS stream on TCP 900.
//!
//! Layout recovered from `ITCMsg::Serialize(BaseStream&, bool)` at `0x5e010` in
//! `libkvmipjproxy.so`, cross-checked against the constructor (`0x5e2ca`),
//! `Init` (`0x5dfce`) and `Reset` (`0x5da42`). See
//! `research/notes/FINDINGS.md` §5.4 and
//! `research/notes/analysis/protocol-framing.disasm.txt`.
//!
//! Endianness is **not** proven by the disassembly — the header is written with
//! a single 32-byte `memcpy`-style `WriteAll`, so the byte order is whatever the
//! i386 client's struct layout is, i.e. little-endian. The adjacent `CommIO`
//! length field *is* explicitly little-endian, which corroborates it.

use crate::Error;

/// Magic in the first header word. `ITCMsg::Reset` writes `0x56ce`.
pub const MAGIC: u32 = 0x0000_56CE;

/// Protocol version in the header. The receiver rejects anything else.
pub const VERSION: u16 = 2;

/// Size of the fixed header that precedes every payload.
pub const HEADER_LEN: usize = 32;

/// Message type, from the `switch` in `ITCMsg::Serialize`.
///
/// Types 1, 2 and 4 carry an opaque byte blob. Type 3 is serialized through an
/// `ITCObject` virtual call rather than written raw; since `ITCMsg` also exposes
/// `SerializeXML`, type 3 is **[ASSUMED]** to be the XML control channel that
/// carries the `<commands>` documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgType {
    /// Raw blob. Exact meaning unconfirmed.
    Raw1,
    /// Raw blob. Exact meaning unconfirmed.
    Raw2,
    /// `ITCObject`-serialized. Likely the XML control channel.
    Object,
    /// Raw blob. Exact meaning unconfirmed.
    Raw4,
}

impl MsgType {
    pub fn from_u16(v: u16) -> Result<Self, Error> {
        Ok(match v {
            1 => MsgType::Raw1,
            2 => MsgType::Raw2,
            3 => MsgType::Object,
            4 => MsgType::Raw4,
            other => return Err(Error::BadMsgType(other)),
        })
    }

    pub fn as_u16(self) -> u16 {
        match self {
            MsgType::Raw1 => 1,
            MsgType::Raw2 => 2,
            MsgType::Object => 3,
            MsgType::Raw4 => 4,
        }
    }
}

/// The 32-byte `ITCMsg` header.
///
/// Field names follow what the disassembly proves. `reserved_*` are words the
/// constructor zeroes and none of the examined code paths write; they are most
/// likely sequence or timestamp fields set by code we have not yet located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Offset 0. Must equal [`MAGIC`].
    pub magic: u32,
    /// Offset 4. Length of the payload that follows the header.
    pub payload_len: u32,
    /// Offset 8. Zeroed by the constructor.
    pub reserved_08: u32,
    /// Offset 12. Zeroed by the constructor.
    pub reserved_0c: u32,
    /// Offset 16. Must equal [`VERSION`].
    pub version: u16,
    /// Offset 18. See [`MsgType`].
    pub msg_type: MsgType,
    /// Offset 20. `ITCMsg::Init` argument 2.
    pub param_a: u32,
    /// Offset 24. `ITCMsg::Init` argument 3.
    pub param_b: u32,
    /// Offset 28. Zeroed by the constructor.
    pub reserved_1c: u32,
}

impl Header {
    pub fn new(msg_type: MsgType, payload_len: u32) -> Self {
        Header {
            magic: MAGIC,
            payload_len,
            reserved_08: 0,
            reserved_0c: 0,
            version: VERSION,
            msg_type,
            param_a: 0,
            param_b: 0,
            reserved_1c: 0,
        }
    }

    /// Parse a header from exactly [`HEADER_LEN`] bytes.
    ///
    /// Validation mirrors the order the device itself uses: version, then
    /// magic, then message type, then a non-zero payload length.
    pub fn parse(buf: &[u8]) -> Result<Self, Error> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Truncated {
                need: HEADER_LEN,
                got: buf.len(),
            });
        }
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(buf[o..o + 2].try_into().unwrap());

        let version = u16_at(16);
        if version != VERSION {
            return Err(Error::BadVersion(version));
        }
        let magic = u32_at(0);
        if magic != MAGIC {
            return Err(Error::BadMagic(magic));
        }
        let msg_type = MsgType::from_u16(u16_at(18))?;
        let payload_len = u32_at(4);
        if payload_len == 0 {
            return Err(Error::EmptyPayload);
        }

        Ok(Header {
            magic,
            payload_len,
            reserved_08: u32_at(8),
            reserved_0c: u32_at(12),
            version,
            msg_type,
            param_a: u32_at(20),
            param_b: u32_at(24),
            reserved_1c: u32_at(28),
        })
    }

    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&self.magic.to_le_bytes());
        b[4..8].copy_from_slice(&self.payload_len.to_le_bytes());
        b[8..12].copy_from_slice(&self.reserved_08.to_le_bytes());
        b[12..16].copy_from_slice(&self.reserved_0c.to_le_bytes());
        b[16..18].copy_from_slice(&self.version.to_le_bytes());
        b[18..20].copy_from_slice(&self.msg_type.as_u16().to_le_bytes());
        b[20..24].copy_from_slice(&self.param_a.to_le_bytes());
        b[24..28].copy_from_slice(&self.param_b.to_le_bytes());
        b[28..32].copy_from_slice(&self.reserved_1c.to_le_bytes());
        b
    }
}

/// A complete `ITCMsg`: header plus payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub header: Header,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn new(msg_type: MsgType, payload: Vec<u8>) -> Self {
        Message {
            header: Header::new(msg_type, payload.len() as u32),
            payload,
        }
    }

    /// Total encoded size.
    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.header.to_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse one message from the front of `buf`.
    ///
    /// Returns the message and how many bytes it consumed, or
    /// [`Error::Truncated`] if `buf` does not yet hold the whole thing — so a
    /// caller can use this directly as a stream decoder.
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), Error> {
        let header = Header::parse(buf)?;
        let total = HEADER_LEN + header.payload_len as usize;
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
    fn header_roundtrip() {
        let h = Header {
            param_a: 0xdead_beef,
            param_b: 7,
            ..Header::new(MsgType::Object, 42)
        };
        assert_eq!(Header::parse(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn header_byte_layout_matches_disassembly() {
        // Offsets asserted against ITCMsg::Serialize @ 0x5e010.
        let b = Header::new(MsgType::Object, 0x1122_3344).to_bytes();
        assert_eq!(&b[0..4], &[0xCE, 0x56, 0x00, 0x00], "magic at offset 0");
        assert_eq!(&b[4..8], &[0x44, 0x33, 0x22, 0x11], "payload_len at offset 4");
        assert_eq!(&b[16..18], &[0x02, 0x00], "version at offset 16");
        assert_eq!(&b[18..20], &[0x03, 0x00], "msg_type at offset 18");
        assert_eq!(b.len(), HEADER_LEN);
    }

    #[test]
    fn rejects_bad_magic_version_and_type() {
        let good = Header::new(MsgType::Raw1, 1).to_bytes();

        let mut b = good;
        b[0] = 0xFF;
        assert!(matches!(Header::parse(&b), Err(Error::BadMagic(_))));

        let mut b = good;
        b[16] = 9;
        assert!(matches!(Header::parse(&b), Err(Error::BadVersion(9))));

        let mut b = good;
        b[18] = 5;
        assert!(matches!(Header::parse(&b), Err(Error::BadMsgType(5))));

        let mut b = good;
        b[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(Header::parse(&b), Err(Error::EmptyPayload)));
    }

    #[test]
    fn version_is_checked_before_magic() {
        // The device validates version first; match that so our errors line up
        // with whatever it would have rejected.
        let mut b = Header::new(MsgType::Raw1, 1).to_bytes();
        b[0] = 0xFF; // bad magic
        b[16] = 9; // bad version
        assert!(matches!(Header::parse(&b), Err(Error::BadVersion(9))));
    }

    #[test]
    fn message_roundtrip_and_stream_decode() {
        let msg = Message::new(MsgType::Raw2, b"hello kvm".to_vec());
        let bytes = msg.to_bytes();
        let (back, n) = Message::parse(&bytes).unwrap();
        assert_eq!(back, msg);
        assert_eq!(n, bytes.len());

        // Two messages back to back decode one at a time.
        let mut stream = bytes.clone();
        stream.extend_from_slice(&Message::new(MsgType::Raw1, vec![1, 2, 3]).to_bytes());
        let (_, n1) = Message::parse(&stream).unwrap();
        let (second, _) = Message::parse(&stream[n1..]).unwrap();
        assert_eq!(second.payload, vec![1, 2, 3]);
    }

    #[test]
    fn truncated_payload_reports_how_much_is_needed() {
        let bytes = Message::new(MsgType::Raw1, vec![0; 100]).to_bytes();
        let err = Message::parse(&bytes[..HEADER_LEN + 10]).unwrap_err();
        assert!(matches!(
            err,
            Error::Truncated {
                need: 132,
                got: 42
            }
        ));
    }
}
