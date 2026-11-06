//! Pairing packet framing: `[version 1][type 1][payload_len BE 4]` then the
//! payload. Version is exactly 1 in both directions.

use crate::error::Error;

pub const VERSION: u8 = 1;
pub const TYPE_SPAKE2_MSG: u8 = 0;
pub const TYPE_PEER_INFO: u8 = 1;
pub const HEADER_LEN: usize = 6;
pub const MAX_PAYLOAD: u32 = 16384;
pub const PEER_INFO_SIZE: usize = 8192;

pub fn encode_header(packet_type: u8, payload_len: usize) -> Result<[u8; HEADER_LEN], Error> {
    if payload_len == 0 || payload_len > MAX_PAYLOAD as usize {
        return Err(Error::Protocol("bad payload length".into()));
    }
    let mut header = [0u8; HEADER_LEN];
    header[0] = VERSION;
    header[1] = packet_type;
    header[2..6].copy_from_slice(&(payload_len as u32).to_be_bytes());
    Ok(header)
}

pub fn decode_header(header: &[u8; HEADER_LEN]) -> Result<(u8, usize), Error> {
    if header[0] != VERSION {
        return Err(Error::Protocol("bad pairing packet version".into()));
    }
    if header[1] != TYPE_SPAKE2_MSG && header[1] != TYPE_PEER_INFO {
        return Err(Error::Protocol("unknown pairing packet type".into()));
    }
    let len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
    if len == 0 || len > MAX_PAYLOAD as usize {
        return Err(Error::Protocol("bad pairing packet length".into()));
    }
    Ok((header[1], len))
}

/// A zero-filled 8192-byte `PeerInfo` carrying our RSA public key.
pub fn peer_info(public_key: &str) -> Result<[u8; PEER_INFO_SIZE], Error> {
    let mut info = [0u8; PEER_INFO_SIZE];
    info[0] = 0; // ADB_RSA_PUB_KEY
    let data = public_key.as_bytes();
    if data.len() >= PEER_INFO_SIZE - 1 {
        return Err(Error::Protocol("public key too long for PeerInfo".into()));
    }
    info[1..1 + data.len()].copy_from_slice(data);
    Ok(info)
}

/// Parse a decrypted `PeerInfo`: returns `(type, data)` with trailing NULs cut.
pub fn parse_peer_info(record: &[u8]) -> Result<(u8, Vec<u8>), Error> {
    if record.len() != PEER_INFO_SIZE {
        return Err(Error::Protocol("PeerInfo has wrong size".into()));
    }
    let kind = record[0];
    let mut data = record[1..].to_vec();
    while data.last() == Some(&0) {
        data.pop();
    }
    Ok((kind, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let header = encode_header(TYPE_SPAKE2_MSG, 32).unwrap();
        assert_eq!(header, [1, 0, 0, 0, 0, 32]);
        assert_eq!(decode_header(&header).unwrap(), (TYPE_SPAKE2_MSG, 32));
    }

    #[test]
    fn header_rejects_garbage() {
        assert!(decode_header(&[2, 0, 0, 0, 0, 5]).is_err());
        assert!(decode_header(&[1, 9, 0, 0, 0, 5]).is_err());
        assert!(decode_header(&[1, 0, 0, 0, 0, 0]).is_err());
        assert!(encode_header(TYPE_SPAKE2_MSG, 0).is_err());
    }

    #[test]
    fn peer_info_is_fixed_size_and_nul_terminated() {
        let info = peer_info("AAAAB3aGVzdABobw==").unwrap();
        assert_eq!(info.len(), PEER_INFO_SIZE);
        assert_eq!(info[0], 0);
        let (kind, data) = parse_peer_info(&info).unwrap();
        assert_eq!(kind, 0);
        assert_eq!(data, b"AAAAB3aGVzdABobw==");
    }
}
