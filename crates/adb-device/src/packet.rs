//! ADB wire protocol packet types.
//!
//! The ADB wire protocol is documented at
//! <https://android.googlesource.com/platform/system/core/+/master/adb/PROTOCOL.txt>
//! (also lives in the AOSP tree).
//!
//! Header layout (24 bytes):
//! ```text
//!   0  command (u32, little-endian)
//!   4  arg0    (u32)
//!   8  arg1    (u32)
//!  12  data_length (u32)
//!  16  data_crc32 (u32)
//!  20  magic   (u32, must equal command^0xFFFFFFFF)
//! ```
//!
//! Commands: A_SYNC, A_CNXN, A_OPEN, A_OKAY, A_CLSE, A_WRTE, A_AUTH.

use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const ADB_VERSION: u32 = 0x01000000;

pub const A_SYNC: u32 = 0x434e5953;
pub const A_CNXN: u32 = 0x4e584e43;
pub const A_OPEN: u32 = 0x4e45504f;
pub const A_OKAY: u32 = 0x59414b4f;
pub const A_CLSE: u32 = 0x45534c43;
pub const A_WRTE: u32 = 0x45545257;
pub const A_AUTH: u32 = 0x48545541;

pub const AUTH_TOKEN: u32 = 1;
pub const AUTH_SIGNATURE: u32 = 2;
pub const AUTH_RSAPUBLICKEY: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Sync,
    Connect,
    Open,
    Okay,
    Close,
    Write,
    Auth,
}

impl Command {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            A_SYNC => Some(Command::Sync),
            A_CNXN => Some(Command::Connect),
            A_OPEN => Some(Command::Open),
            A_OKAY => Some(Command::Okay),
            A_CLSE => Some(Command::Close),
            A_WRTE => Some(Command::Write),
            A_AUTH => Some(Command::Auth),
            _ => None,
        }
    }

    pub fn as_u32(self) -> u32 {
        match self {
            Command::Sync => A_SYNC,
            Command::Connect => A_CNXN,
            Command::Open => A_OPEN,
            Command::Okay => A_OKAY,
            Command::Close => A_CLSE,
            Command::Write => A_WRTE,
            Command::Auth => A_AUTH,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub command: Command,
    pub arg0: u32,
    pub arg1: u32,
    pub payload: Bytes,
}

impl Message {
    pub const HEADER_LEN: usize = 24;

    pub fn new(command: Command, arg0: u32, arg1: u32, payload: Bytes) -> Self {
        Self { command, arg0, arg1, payload }
    }

    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + self.payload.len());
        buf.put_u32_le(self.command.as_u32());
        buf.put_u32_le(self.arg0);
        buf.put_u32_le(self.arg1);
        let len = self.payload.len() as u32;
        buf.put_u32_le(len);
        let crc = data_checksum(&self.payload);
        buf.put_u32_le(crc);
        buf.put_u32_le(self.command.as_u32() ^ 0xFFFF_FFFF);
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn decode(header: &[u8; Self::HEADER_LEN], payload: Bytes) -> crate::Result<Self> {
        let mut h = &header[..];
        let command = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let arg0 = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        let arg1 = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
        let data_length = u32::from_le_bytes([h[12], h[13], h[14], h[15]]);
        let data_crc = u32::from_le_bytes([h[16], h[17], h[18], h[19]]);
        let magic = u32::from_le_bytes([h[20], h[21], h[22], h[23]]);
        let _ = h.get_u32_le();

        let command = Command::from_u32(command)
            .ok_or_else(|| crate::AdbError::InvalidResponse(format!("unknown command 0x{:08x}", command)))?;

        if magic != (command.as_u32() ^ 0xFFFF_FFFF) {
            return Err(crate::AdbError::InvalidResponse("magic mismatch".into()));
        }
        if data_length as usize != payload.len() {
            return Err(crate::AdbError::InvalidResponse(format!(
                "payload length {} != header {}",
                payload.len(),
                data_length
            )));
        }
        let actual_crc = data_checksum(&payload);
        if actual_crc != data_crc {
            return Err(crate::AdbError::InvalidResponse(format!(
                "crc mismatch: got {:08x}, want {:08x}",
                actual_crc, data_crc
            )));
        }

        Ok(Self { command, arg0, arg1, payload })
    }
}

/// ADB's `data_crc32` header field.
///
/// Despite the name, this is *not* a CRC: adbd computes it as a plain additive
/// byte sum (`sum += byte` over the payload, wrapping at 32 bits). See
/// `PROTOCOL.txt` in the AOSP adb tree ("the crc is the sum of all bytes").
pub fn data_checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_checksum_known_answers() {
        // Additive byte sum, so empty input is 0 and "hello" sums to
        // 104+101+108+108+111 = 532 = 0x214.
        assert_eq!(data_checksum(b""), 0);
        assert_eq!(data_checksum(b"hello"), 0x214);
        // "host:version" from the AOSP adb protocol doc.
        assert_eq!(data_checksum(b"host:version"), 1278);
    }

    #[test]
    fn roundtrip_sync() {
        let msg = Message::new(Command::Sync, 1, 0, Bytes::from_static(b"host:version"));
        let encoded = msg.encode();
        let header: [u8; 24] = encoded[..24].try_into().unwrap();
        let decoded = Message::decode(&header, encoded.freeze().slice(24..)).unwrap();
        assert_eq!(decoded.command, Command::Sync);
        assert_eq!(&decoded.payload[..], b"host:version");
    }
}
