//! ADB protocol errors.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AdbError {
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ASN.1/DER error: {0}")]
    Der(#[from] pkcs1::der::Error),

    #[error("ADB protocol error: {0}")]
    Protocol(String),

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("device disconnected")]
    Disconnected,

    #[error("authorization failed: device rejected our key")]
    Unauthorized,

    #[error("timeout waiting for device response")]
    Timeout,

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("serial number mismatch: expected {expected}, got {actual}")]
    SerialMismatch { expected: String, actual: String },

    #[error("transport not supported on this platform")]
    UnsupportedTransport,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AdbError>;
