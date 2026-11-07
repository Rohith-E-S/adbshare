//! Video/audio packet stream parsing (scrcpy wire format).
//!
//! Each stream starts with a 4-byte codec id, then framed packets:
//!
//! ```text
//! session packet (12 bytes): 1sss ssss + width u32 BE + height u32 BE
//! media packet: header (12 bytes) + payload
//!   header byte 0: 0 | config(0x40) | keyframe(0x20) | PTS top 5 bits
//!   bytes 1-7: PTS low bits (61-bit PTS total)
//!   bytes 8-11: payload size u32 BE
//! ```
//!
//! Reference: scrcpy `doc/develop.md` ("Video and audio") and
//! `server/.../device/Streamer.java`.

use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecId {
    H264,
    H265,
    Av1,
    Vp8,
    Vp9,
    Opus,
    Aac,
    Flac,
    Raw,
}

impl CodecId {
    pub fn kind(&self) -> MediaKind {
        match self {
            CodecId::H264 | CodecId::H265 | CodecId::Av1 | CodecId::Vp8 | CodecId::Vp9 => {
                MediaKind::Video
            }
            CodecId::Opus | CodecId::Aac | CodecId::Flac | CodecId::Raw => MediaKind::Audio,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CodecId::H264 => "h264",
            CodecId::H265 => "h265",
            CodecId::Av1 => "av1",
            CodecId::Vp8 => "vp8",
            CodecId::Vp9 => "vp9",
            CodecId::Opus => "opus",
            CodecId::Aac => "aac",
            CodecId::Flac => "flac",
            CodecId::Raw => "raw",
        }
    }
}

/// Parse the 4-byte codec id that opens each stream.
pub fn parse_codec_id(bytes: &[u8]) -> Option<CodecId> {
    if bytes.len() < 4 {
        return None;
    }
    let v = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Some(match v {
        0x68323634 => CodecId::H264,
        0x68323635 => CodecId::H265,
        0x00617631 => CodecId::Av1,
        0x00767038 => CodecId::Vp8,
        0x00767039 => CodecId::Vp9,
        0x6F707573 => CodecId::Opus,
        0x00616163 => CodecId::Aac,
        0x666C6163 => CodecId::Flac,
        0x00726177 => CodecId::Raw,
        _ => return None,
    })
}

/// Length of the device-name field on the first socket.
pub const DEVICE_NAME_FIELD_LENGTH: usize = 64;

/// Length of the video stream header: codec (4 bytes) + width (4 bytes) + height (4 bytes).
pub const VIDEO_HEADER_LENGTH: usize = 12;

/// Parse the 12-byte video stream header: codec (4 bytes), width (4 bytes), height (4 bytes).
pub fn parse_video_header(hdr: &[u8; 12]) -> Option<(CodecId, u32, u32)> {
    let codec = parse_codec_id(&hdr[0..4])?;
    let width = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let height = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    Some((codec, width, height))
}

/// Decode the 64-byte device-name field (UTF-8, NUL-padded).
pub fn parse_device_name(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end.min(DEVICE_NAME_FIELD_LENGTH)]).into_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet {
    /// Capture-session header: carries the frame dimensions. Video only.
    Session {
        width: u32,
        height: u32,
        client_resized: bool,
    },
    /// One encoded media packet (config, keyframe or regular frame).
    Media {
        is_config: bool,
        is_keyframe: bool,
        pts: u64,
        data: Vec<u8>,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unknown codec id")]
    UnknownCodec,
    #[error("packet size {0} exceeds 64 MiB limit")]
    PacketTooLarge(u32),
}

const MAX_PACKET_SIZE: u32 = 64 * 1024 * 1024;

/// Incremental parser: feed arbitrary TCP chunks, get complete packets out.
#[derive(Default)]
pub struct PacketReader {
    buf: Vec<u8>,
}

impl PacketReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every packet that became complete.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Packet>, ProtocolError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 12 {
                break;
            }
            let size = u32::from_be_bytes([self.buf[8], self.buf[9], self.buf[10], self.buf[11]]);
            if size > MAX_PACKET_SIZE {
                return Err(ProtocolError::PacketTooLarge(size));
            }
            if self.buf.len() < 12 + size as usize {
                break;
            }
            let pts_and_flags = u64::from_be_bytes([
                self.buf[0],
                self.buf[1],
                self.buf[2],
                self.buf[3],
                self.buf[4],
                self.buf[5],
                self.buf[6],
                self.buf[7],
            ]);
            let is_config = (pts_and_flags & (1 << 63)) != 0;
            let is_keyframe = (pts_and_flags & (1 << 62)) != 0;
            let pts = pts_and_flags & ((1 << 62) - 1);
            let data = self.buf[12..12 + size as usize].to_vec();
            self.buf.drain(..12 + size as usize);
            out.push(Packet::Media {
                is_config,
                is_keyframe,
                pts,
                data,
            });
        }
        Ok(out)
    }
}

/// A message sent by the device on the control socket (clipboard etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceMessage {
    Clipboard(String),
    AckClipboard(u64),
    UhidOutput { id: u16, data: Vec<u8> },
}

/// Deserialize one device message; returns `(message, bytes consumed)` or
/// `None` when the buffer does not yet hold a complete message.
pub fn parse_device_message(buf: &[u8]) -> Option<(DeviceMessage, usize)> {
    if buf.is_empty() {
        return None;
    }
    match buf[0] {
        0 => {
            // Clipboard: [len u32 BE][text]
            if buf.len() < 5 {
                return None;
            }
            let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
            if len > 1 << 20 || buf.len() < 5 + len {
                return None;
            }
            let text = String::from_utf8_lossy(&buf[5..5 + len]).into_owned();
            Some((DeviceMessage::Clipboard(text), 5 + len))
        }
        1 => {
            if buf.len() < 9 {
                return None;
            }
            let seq = u64::from_be_bytes([
                buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8],
            ]);
            Some((DeviceMessage::AckClipboard(seq), 9))
        }
        2 => {
            if buf.len() < 5 {
                return None;
            }
            let id = u16::from_be_bytes([buf[1], buf[2]]);
            let size = u16::from_be_bytes([buf[3], buf[4]]) as usize;
            if buf.len() < 5 + size {
                return None;
            }
            Some((
                DeviceMessage::UhidOutput {
                    id,
                    data: buf[5..5 + size].to_vec(),
                },
                5 + size,
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_packet(pts: u64, config: bool, keyframe: bool, payload: &[u8]) -> Vec<u8> {
        let mut hdr = vec![0u8; 12];
        let mut flags = pts & ((1 << 62) - 1);
        if config {
            flags |= 1 << 63;
        }
        if keyframe {
            flags |= 1 << 62;
        }
        hdr[0..8].copy_from_slice(&flags.to_be_bytes());
        hdr[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        hdr.extend_from_slice(payload);
        hdr
    }

    #[test]
    fn codec_ids_round_trip() {
        assert_eq!(parse_codec_id(b"h264"), Some(CodecId::H264));
        assert_eq!(parse_codec_id(b"h265"), Some(CodecId::H265));
        assert_eq!(parse_codec_id(b"\0av1"), Some(CodecId::Av1));
        assert_eq!(parse_codec_id(b"opus"), Some(CodecId::Opus));
        assert_eq!(parse_codec_id(b"junk"), None);
    }

    #[test]
    fn device_name_field() {
        let mut field = [0u8; 64];
        field[..5].copy_from_slice(b"Pixel");
        assert_eq!(parse_device_name(&field), "Pixel");
    }

    #[test]
    fn parses_media_packets() {
        let mut reader = PacketReader::new();
        let stream = [
            media_packet(42, true, false, b"sps"),
            media_packet(43, false, true, b"frame"),
        ]
        .concat();
        let packets = reader.feed(&stream).unwrap();
        assert_eq!(
            packets,
            vec![
                Packet::Media {
                    is_config: true,
                    is_keyframe: false,
                    pts: 42,
                    data: b"sps".to_vec()
                },
                Packet::Media {
                    is_config: false,
                    is_keyframe: true,
                    pts: 43,
                    data: b"frame".to_vec()
                },
            ]
        );
    }

    #[test]
    fn parses_across_chunk_boundaries() {
        let mut reader = PacketReader::new();
        let stream = [
            media_packet(7, false, true, b"123456"),
            media_packet(8, false, false, b"789"),
        ]
        .concat();
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            got.extend(reader.feed(chunk).unwrap());
        }
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn device_messages() {
        let (msg, n) = parse_device_message(b"\x00\x00\x00\x00\x05hello").unwrap();
        assert_eq!(msg, DeviceMessage::Clipboard("hello".into()));
        assert_eq!(n, 10);
        assert_eq!(parse_device_message(b"\x00\x00\x00"), None);
    }
}
