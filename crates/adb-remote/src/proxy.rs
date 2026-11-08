//! GUI ↔ daemon proxy framing.
//!
//! The daemon owns the ADB tunnels and the server process; the GUI never
//! touches ADB. They communicate over a per-device Unix socket using
//! u32-BE-length-prefixed frames. Each frame payload starts with a tag byte:
//!
//! ```text
//! daemon → GUI: TAG_META (4-byte codec id) | TAG_VIDEO (raw packet bytes)
//!               | TAG_CONTROL (device message: clipboard etc.)
//!               | TAG_FRAME (shared memory frame arrival notification)
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

/// Frame tag for daemon → GUI shared-memory frame arrival notifications.
pub const TAG_FRAME: u8 = b'F';

/// Total byte size of a serialized [`FrameNotification`] payload including the [`TAG_FRAME`] tag byte.
///
/// Layout (29 bytes total):
/// - `[0..1]`: [`TAG_FRAME`] (`b'F'`, 1 byte)
/// - `[1..9]`: `seq` (u64 BE, 8 bytes)
/// - `[9..13]`: `slot` (u32 BE, 4 bytes)
/// - `[13..17]`: `width` (u32 BE, 4 bytes)
/// - `[17..21]`: `height` (u32 BE, 4 bytes)
/// - `[21..29]`: `timestamp_ns` (u64 BE, 8 bytes)
pub const FRAME_NOTIFICATION_SIZE: usize = 29;

/// Size of the frame notification body excluding the [`TAG_FRAME`] tag byte.
pub const FRAME_NOTIFICATION_BODY_SIZE: usize = FRAME_NOTIFICATION_SIZE - 1; // 28 bytes

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

/// Notification sent by `adb-daemon` over the Unix domain socket when a newly decoded
/// video frame has been published into a shared-memory buffer slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameNotification {
    /// Tag identifier (always [`TAG_FRAME`]).
    pub tag: u8,
    /// Monotonically increasing sequence number (matches `ShmHeader.head_seq`).
    pub seq: u64,
    /// Ring-buffer slot index in shared memory containing this frame (matches `ShmHeader.head_slot`).
    pub slot: u32,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Timestamp in nanoseconds (monotonic or UNIX epoch) when the frame was published.
    pub timestamp_ns: u64,
}

impl FrameNotification {
    pub const SIZE: usize = FRAME_NOTIFICATION_SIZE;
    pub const BODY_SIZE: usize = FRAME_NOTIFICATION_BODY_SIZE;

    /// Construct a new frame notification with an explicit timestamp.
    pub const fn new(seq: u64, slot: u32, width: u32, height: u32, timestamp_ns: u64) -> Self {
        Self {
            tag: TAG_FRAME,
            seq,
            slot,
            width,
            height,
            timestamp_ns,
        }
    }

    /// Construct a new frame notification with `timestamp_ns` set to the current system time.
    pub fn now(seq: u64, slot: u32, width: u32, height: u32) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self::new(seq, slot, width, height, timestamp_ns)
    }

    /// Serialize into a fixed-size byte array prefixed with [`TAG_FRAME`]. Zero heap allocation.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0] = self.tag;
        b[1..9].copy_from_slice(&self.seq.to_be_bytes());
        b[9..13].copy_from_slice(&self.slot.to_be_bytes());
        b[13..17].copy_from_slice(&self.width.to_be_bytes());
        b[17..21].copy_from_slice(&self.height.to_be_bytes());
        b[21..29].copy_from_slice(&self.timestamp_ns.to_be_bytes());
        b
    }

    /// Alias for `to_bytes()` returning fixed-size array compatible with slice/deref expectations.
    #[inline]
    pub fn serialize(&self) -> [u8; Self::SIZE] {
        self.to_bytes()
    }

    /// Serialize into a newly allocated [`Vec<u8>`] prefixed with [`TAG_FRAME`].
    pub fn to_vec(&self) -> Vec<u8> {
        self.to_bytes().to_vec()
    }

    /// Serialize into a length-prefixed frame ready to be sent over the Unix socket.
    ///
    /// Produces 33 bytes: 4 bytes BE length prefix (value 29) + 29 bytes payload.
    pub fn to_frame(&self) -> Vec<u8> {
        frame(&self.to_bytes())
    }

    /// Parse a frame notification from a payload slice starting with [`TAG_FRAME`].
    ///
    /// Returns `Some(FrameNotification)` if `buf.len() >= Self::SIZE` and `buf[0] == TAG_FRAME`.
    /// Trailing bytes are ignored for forward compatibility.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE || buf[0] != TAG_FRAME {
            return None;
        }
        Self::parse_body(&buf[1..])
    }

    /// Parse a frame notification from the 28-byte body (excluding [`TAG_FRAME`]).
    ///
    /// Returns `Some(FrameNotification)` if `body.len() >= Self::BODY_SIZE`.
    pub fn parse_body(body: &[u8]) -> Option<Self> {
        if body.len() < Self::BODY_SIZE {
            return None;
        }
        let seq = u64::from_be_bytes(body[0..8].try_into().ok()?);
        let slot = u32::from_be_bytes(body[8..12].try_into().ok()?);
        let width = u32::from_be_bytes(body[12..16].try_into().ok()?);
        let height = u32::from_be_bytes(body[16..20].try_into().ok()?);
        let timestamp_ns = u64::from_be_bytes(body[20..28].try_into().ok()?);
        Some(Self {
            tag: TAG_FRAME,
            seq,
            slot,
            width,
            height,
            timestamp_ns,
        })
    }
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

    /// Feed bytes; returns every complete frame. Oversize headers clear the
    /// buffer (un-wedge) so one bad frame does not DoS the connection.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        if self.buf.len().saturating_add(chunk.len()) > (MAX_FRAME as usize) + 4 {
            // Unbounded growth from incomplete/oversize frames: drop it.
            self.buf.clear();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
            if len > MAX_FRAME {
                self.buf.clear();
                break;
            }
            if self.buf.len() < 4 + len as usize {
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

    #[test]
    fn frame_notification_round_trip() {
        let notif = FrameNotification::new(1001, 2, 1920, 1080, 1_700_000_000_123_456_789);
        let bytes = notif.to_bytes();
        assert_eq!(bytes.len(), FrameNotification::SIZE);
        assert_eq!(bytes[0], TAG_FRAME);

        let parsed = FrameNotification::parse(&bytes).expect("parse failed");
        assert_eq!(parsed, notif);
        assert_eq!(parsed.tag, TAG_FRAME);
        assert_eq!(parsed.seq, 1001);
        assert_eq!(parsed.slot, 2);
        assert_eq!(parsed.width, 1920);
        assert_eq!(parsed.height, 1080);
        assert_eq!(parsed.timestamp_ns, 1_700_000_000_123_456_789);
    }

    #[test]
    fn frame_notification_wire_exact_bytes() {
        let notif = FrameNotification {
            tag: TAG_FRAME,
            seq: 0x0102030405060708,
            slot: 0x11223344,
            width: 0x55667788,
            height: 0x12345678,
            timestamp_ns: 0x0807060504030201,
        };
        let b = notif.to_bytes();
        assert_eq!(b[0], b'F');
        assert_eq!(&b[1..9], &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(&b[9..13], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&b[13..17], &[0x55, 0x66, 0x77, 0x88]);
        assert_eq!(&b[17..21], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(
            &b[21..29],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
    }

    #[test]
    fn frame_notification_parse_errors() {
        assert_eq!(FrameNotification::parse(&[]), None);
        assert_eq!(FrameNotification::parse(&[TAG_FRAME; 28]), None);

        let mut wrong_tag = [0u8; FrameNotification::SIZE];
        wrong_tag[0] = b'M';
        assert_eq!(FrameNotification::parse(&wrong_tag), None);

        assert_eq!(FrameNotification::parse_body(&[0u8; 27]), None);
    }

    #[test]
    fn frame_notification_allows_trailing_bytes() {
        let notif = FrameNotification::new(42, 1, 1080, 2400, 999_999);
        let mut extended = notif.to_bytes().to_vec();
        extended.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        let parsed =
            FrameNotification::parse(&extended).expect("should parse despite trailing bytes");
        assert_eq!(parsed, notif);
    }

    #[test]
    fn frame_notification_with_frame_reader() {
        let n1 = FrameNotification::new(1, 0, 1080, 1920, 1_000_000);
        let n2 = FrameNotification::new(2, 1, 1080, 1920, 2_000_000);
        let n3 = FrameNotification::new(3, 2, 1080, 1920, 3_000_000);

        let stream = [n1.to_frame(), n2.to_frame(), n3.to_frame()].concat();
        assert_eq!(stream.len(), 3 * 33);

        let mut reader = FrameReader::new();
        let mut parsed_notifications = Vec::new();
        for chunk in stream.chunks(5) {
            for payload in reader.feed(chunk) {
                if let Some(notif) = FrameNotification::parse(&payload) {
                    parsed_notifications.push(notif);
                }
            }
        }

        assert_eq!(parsed_notifications, vec![n1, n2, n3]);
    }

    #[test]
    fn interleaved_proxy_stream() {
        let meta = frame(&[TAG_META, 0x68, 0x32, 0x36, 0x34]);
        let frame_notif = FrameNotification::new(7, 0, 1280, 720, 500_000);
        let control = frame(&[TAG_CONTROL, 0x01, 0x00, 0x00]);

        let stream = [meta, frame_notif.to_frame(), control].concat();

        let mut reader = FrameReader::new();
        let frames = reader.feed(&stream);
        assert_eq!(frames.len(), 3);

        assert_eq!(frames[0][0], TAG_META);
        assert_eq!(frames[1][0], TAG_FRAME);
        assert_eq!(FrameNotification::parse(&frames[1]), Some(frame_notif));
        assert_eq!(frames[2][0], TAG_CONTROL);
    }

    #[test]
    fn frame_notification_now_has_nonzero_timestamp() {
        let notif = FrameNotification::now(1, 0, 1080, 1920);
        assert!(notif.timestamp_ns > 0);
    }
}
