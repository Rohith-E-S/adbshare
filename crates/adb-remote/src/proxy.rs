//! GUI ↔ daemon proxy framing.
//!
//! The daemon owns the ADB tunnels and the server process; the GUI never
//! touches ADB. They communicate over a per-device Unix socket using
//! u32-BE-length-prefixed frames. Each frame payload starts with a tag byte:
//!
//! ```text
//! daemon → GUI: TAG_META (4-byte codec id) | TAG_VIDEO (raw packet bytes)
//!               | TAG_CONTROL (device message: clipboard etc.)
//! gui → daemon: scrcpy control messages (no tag)
//! ```
//!
//! Video bytes are forwarded verbatim (after the tag) so the GUI can feed
//! its own [`crate::protocol::PacketReader`]; dimension changes arrive as
//! session packets inside that stream.

/// Frame tags for daemon → GUI messages.
pub const TAG_META: u8 = b'M';
pub const TAG_VIDEO: u8 = b'V';
pub const TAG_CONTROL: u8 = b'C';

/// Maximum framed message size on the proxy socket (256 KiB, matching
/// scrcpy's `SC_CONTROL_MSG_MAX_SIZE`).
pub const MAX_FRAME: u32 = 1 << 18;

/// Frame one message with a u32 BE length prefix.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Incremental decoder for length-prefixed frames.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every complete frame.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
            if len > MAX_FRAME || self.buf.len() < 4 + len as usize {
                break;
            }
            out.push(self.buf[4..4 + len as usize].to_vec());
            self.buf.drain(..4 + len as usize);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_across_chunks() {
        let wire = [frame(b"abc"), frame(b""), frame(b"hello world")].concat();
        let mut reader = FrameReader::new();
        let mut got = Vec::new();
        for chunk in wire.chunks(2) {
            got.extend(reader.feed(chunk));
        }
        assert_eq!(got, vec![b"abc".to_vec(), vec![], b"hello world".to_vec()]);
    }

    #[test]
    fn incomplete_frame_waits() {
        let mut reader = FrameReader::new();
        assert!(reader.feed(&frame(b"data")[..5]).is_empty());
        assert_eq!(reader.feed(&frame(b"data")[5..]), vec![b"data".to_vec()]);
    }
}
