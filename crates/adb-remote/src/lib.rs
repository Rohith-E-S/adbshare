//! Real-time remote control wire protocol.
//!
//! This crate implements the client side of scrcpy's server protocol
//! (Apache-2.0, Genymobile): separate video/audio/control sockets opened over
//! `adb reverse`/`adb forward`, a 4-byte codec id per stream, 12-byte packet
//! headers, and a binary control-message format. See
//! https://github.com/Genymobile/scrcpy/blob/master/doc/develop.md
//!
//! The protocol is version-locked: the client must be run with a matching
//! server version, exactly as upstream requires. [`SERVER_VERSION`] is the
//! version string passed to the server; override with `ADBSHARE_SCRCPY_VERSION`
//! if the bundled server jar is a different release.

pub mod input;
pub mod protocol;
pub mod proxy;
pub mod shm;

pub use input::ControlMessage;
pub use protocol::{
    CodecId, DeviceMessage, Packet, PacketReader, ProtocolError, parse_codec_id,
    parse_codec_id_with_status, parse_device_message, parse_device_name,
};
pub use proxy::{
    FRAME_NOTIFICATION_BODY_SIZE, FRAME_NOTIFICATION_SIZE, FrameNotification, FrameReader,
    MAX_FRAME, TAG_CONTROL, TAG_FRAME, TAG_META, TAG_VIDEO, frame,
};
pub use shm::{
    DEFAULT_NUM_SLOTS, DEFAULT_SLOT_SIZE, FORMAT_BGRA8888, FORMAT_RGBA8888, FrameSnapshot,
    SHM_HEADER_SIZE, SHM_MAGIC, SHM_VERSION, ShmError, ShmHeader, ShmPool, ShmReader, ShmWriter,
};

/// Version string passed to the server's `main()` (must match the jar).
pub const SERVER_VERSION: &str = "3.3";

/// Environment variable overriding [`SERVER_VERSION`] when the bundled jar is
/// a different release (e.g. `ADBSHARE_SCRCPY_VERSION=4.0`).
pub const SERVER_VERSION_ENV: &str = "ADBSHARE_SCRCPY_VERSION";

/// Server main class executed via `app_process`.
pub const SERVER_MAIN_CLASS: &str = "com.genymobile.scrcpy.Server";

/// Where the server jar is pushed on the phone (shell-writable, not
/// world-writable, so a malicious app cannot swap it before execution).
pub const SERVER_REMOTE_PATH: &str = "/data/local/tmp/adbshare-scrcpy-server.jar";

/// Abstract socket name prefix. Must match the scrcpy server's hard-coded
/// `SOCKET_NAME_PREFIX = "scrcpy"` (see DesktopConnection.java `getSocketName`).
pub const SOCKET_NAME_PREFIX: &str = "scrcpy";

/// A quality preset for the video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualityPreset {
    /// Name shown in the UI.
    pub name: &'static str,
    /// Server `max_size` (longest edge, pixels).
    pub max_size: u32,
    /// Server `max_fps`. Set to 0 for unlimited refresh rate.
    pub max_fps: u32,
    /// Server `video_bit_rate` in bits/sec.
    pub bit_rate: u32,
}

impl QualityPreset {
    /// Create a custom quality preset.
    pub const fn custom(name: &'static str, max_size: u32, max_fps: u32, bit_rate: u32) -> Self {
        Self {
            name,
            max_size,
            max_fps,
            bit_rate,
        }
    }

    /// Return a copy of this preset with an overridden frame rate.
    /// Set `max_fps: 0` for unlimited refresh rate.
    pub const fn with_max_fps(mut self, max_fps: u32) -> Self {
        self.max_fps = max_fps;
        self
    }

    /// Check if this preset has no frame rate limit (unlimited FPS).
    pub const fn is_unlimited_fps(&self) -> bool {
        self.max_fps == 0
    }
}

/// Upgraded to 60 FPS standard
pub const PRESET_PERFORMANCE: QualityPreset = QualityPreset {
    name: "Performance",
    max_size: 1280,
    max_fps: 60,
    bit_rate: 4_000_000,
};

pub const PRESET_BALANCED: QualityPreset = QualityPreset {
    name: "Balanced",
    max_size: 1920,
    max_fps: 60,
    bit_rate: 8_000_000,
};

pub const PRESET_QUALITY: QualityPreset = QualityPreset {
    name: "Quality",
    max_size: 2560,
    max_fps: 60,
    bit_rate: 16_000_000,
};

/// High refresh rate presets
pub const PRESET_HIGH_REFRESH_90: QualityPreset = QualityPreset {
    name: "90 FPS",
    max_size: 1920,
    max_fps: 90,
    bit_rate: 12_000_000,
};

pub const PRESET_HIGH_REFRESH_120: QualityPreset = QualityPreset {
    name: "120 FPS",
    max_size: 1920,
    max_fps: 120,
    bit_rate: 16_000_000,
};

pub const PRESET_UNLIMITED: QualityPreset = QualityPreset {
    name: "Unlimited",
    max_size: 1920,
    max_fps: 0,
    bit_rate: 12_000_000,
};

pub const PRESETS: &[QualityPreset] = &[PRESET_PERFORMANCE, PRESET_BALANCED, PRESET_QUALITY];

/// Options for starting a remote session (serialised as JSON over D-Bus).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOptions {
    pub preset: QualityPreset,
    pub audio: bool,
    pub server_version: String,
    pub shm_path: Option<String>,
}

impl SessionOptions {
    pub fn new(preset: QualityPreset) -> Self {
        Self {
            preset,
            audio: true,
            server_version: server_version(),
            shm_path: None,
        }
    }

    /// Enable or disable audio forwarding.
    pub fn with_audio(mut self, audio: bool) -> Self {
        self.audio = audio;
        self
    }

    /// Set an explicit shared memory file path.
    pub fn with_shm_path(mut self, shm_path: impl Into<String>) -> Self {
        self.shm_path = Some(shm_path.into());
        self
    }

    /// Override the frame rate limit for this session (e.g. 60, 90, 120, or 0 for unlimited).
    pub fn with_max_fps(mut self, max_fps: u32) -> Self {
        self.preset.max_fps = max_fps;
        self
    }

    /// Check if this session's preset has unlimited FPS.
    pub fn is_unlimited_fps(&self) -> bool {
        self.preset.is_unlimited_fps()
    }
}

/// The version string to pass to the server, honouring the env override.
pub fn server_version() -> String {
    std::env::var(SERVER_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SERVER_VERSION.to_string())
}

/// Generate a random 31-bit session id (`scid`), as scrcpy does, so several
/// sessions can target the same device without socket-name collisions.
pub fn random_scid() -> i32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x9e3779b9);
    let mut x = nanos ^ (std::process::id() as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
    // xorshift64*
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    ((v >> 32) & 0x7fff_ffff) as i32
}

/// Abstract socket name for a session (`scrcpy_<scid>` as 8 hex digits).
pub fn socket_name(scid: i32) -> String {
    format!("{SOCKET_NAME_PREFIX}_{scid:08x}")
}

/// Live stream statistics reported by the daemon.
#[derive(Clone, Debug, Default)]
pub struct RemoteStats {
    pub frames: u64,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub fps_x10: u32,
    pub uptime_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_standard_60fps() {
        assert_eq!(PRESET_PERFORMANCE.max_fps, 60);
        assert_eq!(PRESET_BALANCED.max_fps, 60);
        assert_eq!(PRESET_QUALITY.max_fps, 60);
    }

    #[test]
    fn high_refresh_rate_presets() {
        assert_eq!(PRESET_HIGH_REFRESH_90.max_fps, 90);
        assert_eq!(PRESET_HIGH_REFRESH_120.max_fps, 120);
        assert_eq!(PRESET_UNLIMITED.max_fps, 0);
        assert!(PRESET_UNLIMITED.is_unlimited_fps());
        assert!(!PRESET_PERFORMANCE.is_unlimited_fps());
    }

    #[test]
    fn preset_builders() {
        let preset = PRESET_BALANCED.with_max_fps(120);
        assert_eq!(preset.max_fps, 120);
        assert_eq!(preset.max_size, 1920);

        let unlimited = PRESET_QUALITY.with_max_fps(0);
        assert!(unlimited.is_unlimited_fps());
    }

    #[test]
    fn session_options_builders() {
        let default_opts = SessionOptions::new(PRESET_PERFORMANCE);
        assert!(default_opts.audio);
        assert_eq!(default_opts.shm_path, None);

        let opts = SessionOptions::new(PRESET_PERFORMANCE)
            .with_audio(false)
            .with_shm_path("/run/user/1000/custom.shm")
            .with_max_fps(90);
        assert_eq!(opts.preset.max_fps, 90);
        assert!(!opts.audio);
        assert_eq!(opts.shm_path.as_deref(), Some("/run/user/1000/custom.shm"));
        assert!(!opts.is_unlimited_fps());

        let opts_unlimited = opts.with_max_fps(0);
        assert!(opts_unlimited.is_unlimited_fps());
    }
}
