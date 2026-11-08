//! Test harness and mock models for Opaque-Box E2E Testing.
//!
//! Provides protocol framing, shared memory layout validation, mock Unix domain
//! socket endpoints, audio negotiation handlers, and clipboard echo suppression
//! models conforming strictly to `PROJECT.md § Interface Contracts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::sync::Mutex;

/// Magic identifier for ADBShare Shared Memory (`"ADBS"`).
pub const SHM_MAGIC: u32 = 0x41444253;
/// Protocol version for Shared Memory Header.
pub const SHM_VERSION: u32 = 1;
/// Number of slots in the triple buffer.
pub const SHM_NUM_SLOTS: usize = 3;
/// Maximum supported slot size (2560 x 2560 x 4 = ~26 MB).
pub const MAX_SLOT_SIZE: usize = 2560 * 2560 * 4;

/// Tag byte for 25-byte frame notifications.
pub const TAG_FRAME: u8 = b'F';
/// Tag byte for metadata.
pub const TAG_META: u8 = b'M';
/// Tag byte for raw video.
pub const TAG_VIDEO: u8 = b'V';
/// Tag byte for control / device messages.
pub const TAG_CONTROL: u8 = b'C';

/// 25-byte Frame Notification payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameNotification {
    pub tag: u8,
    pub seq: u64,
    pub slot: u32,
    pub width: u32,
    pub height: u32,
    pub timestamp_ns: u64,
}

pub const FRAME_NOTIFICATION_SIZE: usize = 29;

impl FrameNotification {
    pub fn new(seq: u64, slot: u32, width: u32, height: u32, timestamp_ns: u64) -> Self {
        Self {
            tag: TAG_FRAME,
            seq,
            slot,
            width,
            height,
            timestamp_ns,
        }
    }

    /// Serialise to 29-byte binary representation according to PROJECT.md and M1 spec.
    pub fn serialize(&self) -> [u8; FRAME_NOTIFICATION_SIZE] {
        let mut buf = [0u8; FRAME_NOTIFICATION_SIZE];
        buf[0] = self.tag;
        buf[1..9].copy_from_slice(&self.seq.to_be_bytes());
        buf[9..13].copy_from_slice(&self.slot.to_be_bytes());
        buf[13..17].copy_from_slice(&self.width.to_be_bytes());
        buf[17..21].copy_from_slice(&self.height.to_be_bytes());
        buf[21..29].copy_from_slice(&self.timestamp_ns.to_be_bytes());
        buf
    }

    /// Parse from binary representation.
    pub fn parse(buf: &[u8]) -> Result<Self, &'static str> {
        if buf.len() < FRAME_NOTIFICATION_SIZE {
            return Err("buffer too short, expected at least 29 bytes");
        }
        if buf.len() > FRAME_NOTIFICATION_SIZE {
            return Err("buffer exceeds 29 bytes for frame notification");
        }
        if buf[0] != TAG_FRAME {
            return Err("invalid tag byte, expected TAG_FRAME (b'F')");
        }
        let seq = u64::from_be_bytes(buf[1..9].try_into().unwrap());
        let slot = u32::from_be_bytes(buf[9..13].try_into().unwrap());
        let width = u32::from_be_bytes(buf[13..17].try_into().unwrap());
        let height = u32::from_be_bytes(buf[17..21].try_into().unwrap());
        let timestamp_ns = u64::from_be_bytes(buf[21..29].try_into().unwrap());
        Ok(Self {
            tag: buf[0],
            seq,
            slot,
            width,
            height,
            timestamp_ns,
        })
    }
}

/// Simulated Shared Memory Region with Triple Buffering conforming to `PROJECT.md § 1`.
pub struct MockShmPool {
    pub magic: u32,
    pub version: u32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub stride: AtomicU32,
    pub active_slot: AtomicU32,
    pub seq: AtomicU64,
    slots: [Vec<u8>; SHM_NUM_SLOTS],
    pub slot_capacity: usize,
}

impl MockShmPool {
    pub fn new(slot_capacity: usize) -> Result<Self, &'static str> {
        if slot_capacity == 0 || slot_capacity > MAX_SLOT_SIZE {
            return Err("invalid slot capacity");
        }
        Ok(Self {
            magic: SHM_MAGIC,
            version: SHM_VERSION,
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            stride: AtomicU32::new(0),
            active_slot: AtomicU32::new(0),
            seq: AtomicU64::new(0),
            slots: [
                vec![0u8; slot_capacity],
                vec![0u8; slot_capacity],
                vec![0u8; slot_capacity],
            ],
            slot_capacity,
        })
    }

    /// Publish a frame into a target slot.
    pub fn publish_frame(
        &mut self,
        slot: u32,
        w: u32,
        h: u32,
        frame_data: &[u8],
    ) -> Result<u64, &'static str> {
        if w == 0 || h == 0 {
            return Err("dimensions must be non-zero");
        }
        let needed_bytes = (w as usize) * (h as usize) * 4;
        if needed_bytes > self.slot_capacity {
            return Err("frame exceeds slot capacity");
        }
        if (slot as usize) >= SHM_NUM_SLOTS {
            return Err("slot index out of bounds");
        }
        if frame_data.len() != needed_bytes {
            return Err("frame data size does not match w * h * 4");
        }

        self.slots[slot as usize][..needed_bytes].copy_from_slice(frame_data);
        self.width.store(w, Ordering::Release);
        self.height.store(h, Ordering::Release);
        self.stride.store(w * 4, Ordering::Release);
        self.active_slot.store(slot, Ordering::Release);
        let new_seq = self.seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        Ok(new_seq)
    }

    /// Read published frame from a slot.
    pub fn read_slot(&self, slot: usize) -> Result<&[u8], &'static str> {
        if slot >= SHM_NUM_SLOTS {
            return Err("slot out of bounds");
        }
        let w = self.width.load(Ordering::Acquire) as usize;
        let h = self.height.load(Ordering::Acquire) as usize;
        let bytes = w * h * 4;
        if bytes == 0 || bytes > self.slot_capacity {
            return Err("uninitialized or invalid frame dimensions");
        }
        Ok(&self.slots[slot][..bytes])
    }
}

/// Simulated Audio Handshake & Codec Negotiator.
pub struct MockAudioNegotiator;

impl MockAudioNegotiator {
    /// Evaluate 4-byte audio header:
    /// Returns Ok(Some(codec)) for valid audio,
    /// Ok(None) for code 0 (fallback),
    /// Err for invalid / unknown headers.
    pub fn evaluate_header(
        header: &[u8],
    ) -> Result<Option<adb_remote::protocol::CodecId>, &'static str> {
        if header.len() < 4 {
            return Err("header too short");
        }
        let code = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        if code == 0x00000000 {
            return Ok(None); // Graceful fallback on code 0
        }
        match adb_remote::protocol::parse_codec_id(&header[..4]) {
            Some(codec) => Ok(Some(codec)),
            None => Err("unknown or invalid audio codec"),
        }
    }

    /// Construct host audio sink command line.
    pub fn build_sink_command(sink_type: &str) -> Vec<String> {
        match sink_type {
            "pw-cat" => vec![
                "pw-cat".into(),
                "-p".into(),
                "-a".into(),
                "--format".into(),
                "s16".into(),
                "--rate".into(),
                "48000".into(),
                "--channels".into(),
                "2".into(),
                "-".into(),
            ],
            "pacat" => vec![
                "pacat".into(),
                "--playback".into(),
                "--raw".into(),
                "--format=s16ne".into(),
                "--rate=48000".into(),
                "--channels=2".into(),
                "-".into(),
            ],
            "ffmpeg" => vec![
                "ffmpeg".into(),
                "-f".into(),
                "s16le".into(),
                "-ar".into(),
                "48000".into(),
                "-ac".into(),
                "2".into(),
                "-i".into(),
                "pipe:0".into(),
                "-f".into(),
                "pulse".into(),
                "ADBShare".into(),
            ],
            _ => vec![],
        }
    }
}

/// Clipboard Synchronizer with Echo Suppression.
pub struct MockClipboardBridge {
    last_host_seq: AtomicU64,
    last_device_text: Arc<Mutex<Option<String>>>,
}

impl Default for MockClipboardBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl MockClipboardBridge {
    pub fn new() -> Self {
        Self {
            last_host_seq: AtomicU64::new(0),
            last_device_text: Arc::new(Mutex::new(None)),
        }
    }

    /// Called when host copies text -> emits SetClipboard control message.
    pub fn on_host_copy(&self, text: &str) -> (u64, adb_remote::ControlMessage) {
        let seq = self.last_host_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let msg = adb_remote::ControlMessage::SetClipboard {
            sequence: seq,
            paste: false,
            text: text.to_string(),
        };
        (seq, msg)
    }

    /// Called when device sends Clipboard message via TAG_CONTROL.
    /// Returns true if accepted (new text), or false if suppressed (echo).
    pub async fn on_device_clipboard(&self, text: &str) -> bool {
        let mut guard = self.last_device_text.lock().await;
        if guard.as_deref() == Some(text) {
            return false; // Suppress echo
        }
        *guard = Some(text.to_string());
        true
    }
}

/// Drag and Drop File Classifier.
pub struct MockDragDropRouter;

#[derive(Debug, PartialEq, Eq)]
pub enum DropAction {
    InstallApk(std::path::PathBuf),
    PushFile {
        source: std::path::PathBuf,
        destination: String,
    },
    Invalid(String),
}

impl MockDragDropRouter {
    pub fn route_drop(path_str: &str) -> DropAction {
        let path = std::path::Path::new(path_str);
        if path_str.trim().is_empty() {
            return DropAction::Invalid("empty path".into());
        }
        if path_str.to_lowercase().ends_with(".apk") {
            return DropAction::InstallApk(path.to_path_buf());
        }
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let destination = format!("/sdcard/Download/{file_name}");
        DropAction::PushFile {
            source: path.to_path_buf(),
            destination,
        }
    }
}
