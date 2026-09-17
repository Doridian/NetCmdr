//! `CommIO` — the virtual-media / USB-over-IP packet framing.
//!
//! Layout recovered from `CommIO::sendPacket(u8, u8, const u8*, u32)` at
//! `0x843dc` and `CommIO::readPacket(u8&, buf&, u32)` at `0x84836` in
//! `libkvmipjproxy.so`. See `research/notes/FINDINGS.md` §5.4.
//!
//! This layer carries `RemoteINEndpoint` / `RemoteOUTEndpoint` traffic and
//! `MassStorageInterface::onPacket`, i.e. mounted ISOs and drives appear to the
//! target as USB mass storage driven over this channel.

use crate::Error;

/// Size of the packet header.
pub const HEADER_LEN: usize = 4;

/// Largest payload the sender will encode.
///
/// `sendPacket` throws `cException(9)` when `len >> 16` is non-zero, so the
/// length must fit in the 16-bit field.
pub const MAX_PAYLOAD: usize = u16::MAX as usize;

/// A `CommIO` packet.
///
/// The two leading bytes are passed separately to `sendPacket`; only the second
/// is handed back by `readPacket`, which suggests `channel` selects an endpoint
/// and `opcode` selects an operation within it. Both are **[ASSUMED]** names —
/// the disassembly proves the layout, not the semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// Header byte 0 — `sendPacket` argument 1.
    pub channel: u8,
    /// Header byte 1 — `sendPacket` argument 2. This is the byte `readPacket`
    /// returns to its caller.
    pub opcode: u8,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn new(channel: u8, opcode: u8, payload: Vec<u8>) -> Result<Self, Error> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge(payload.len()));
        }
        Ok(Packet {
            channel,
            opcode,
            payload,
        })
    }

    pub fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let len = self.payload.len() as u16;
        let mut out = Vec::with_capacity(self.encoded_len());
        out.push(self.channel);
        out.push(self.opcode);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse one packet from the front of `buf`, returning it and the number of
    /// bytes consumed.
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), Error> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Truncated {
                need: HEADER_LEN,
                got: buf.len(),
            });
        }
        // readPacket: len = hdr[2] | hdr[3] << 8
        let len = u16::from_le_bytes([buf[2], buf[3]]) as usize;
        let total = HEADER_LEN + len;
        if buf.len() < total {
            return Err(Error::Truncated {
                need: total,
                got: buf.len(),
            });
        }
        Ok((
            Packet {
                channel: buf[0],
                opcode: buf[1],
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
    fn header_byte_layout_matches_disassembly() {
        let p = Packet::new(0xAA, 0xBB, vec![0u8; 0x1234]).unwrap();
        let b = p.to_bytes();
        assert_eq!(b[0], 0xAA, "channel at offset 0");
        assert_eq!(b[1], 0xBB, "opcode at offset 1");
        // length little-endian: hdr[2] = low byte, hdr[3] = high byte
        assert_eq!(b[2], 0x34);
        assert_eq!(b[3], 0x12);
    }

    #[test]
    fn empty_payload_writes_zero_length() {
        // sendPacket with a NULL data pointer writes both length bytes as zero.
        let b = Packet::new(1, 2, Vec::new()).unwrap().to_bytes();
        assert_eq!(b, vec![1, 2, 0, 0]);
    }

    #[test]
    fn roundtrip() {
        let p = Packet::new(3, 4, b"payload".to_vec()).unwrap();
        let (back, n) = Packet::parse(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
        assert_eq!(n, 4 + 7);
    }

    #[test]
    fn rejects_oversized_payload() {
        let err = Packet::new(0, 0, vec![0u8; MAX_PAYLOAD + 1]).unwrap_err();
        assert!(matches!(err, Error::PayloadTooLarge(_)));
    }

    #[test]
    fn max_payload_is_encodable() {
        let p = Packet::new(0, 0, vec![0u8; MAX_PAYLOAD]).unwrap();
        let b = p.to_bytes();
        assert_eq!(&b[2..4], &[0xFF, 0xFF]);
        assert_eq!(Packet::parse(&b).unwrap().0.payload.len(), MAX_PAYLOAD);
    }
}
