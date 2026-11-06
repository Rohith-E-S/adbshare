//! Client for Android's wireless ADB pairing protocol.
//!
//! The pairing client speaks TLS 1.3 with the host's ADB key as client
//! certificate, derives SPAKE2 material from the TLS exporter, and hands
//! over `PeerInfo` (our public key) encrypted with an HKDF-derived
//! AES-128-GCM transport. Used by the Studio-style QR flow: generate a
//! payload, let the phone scan it, discover the phone's pairing server over
//! mDNS, pair, then connect over `_adb-tls-connect`.

pub mod cipher;
pub mod client;
pub mod error;
pub mod mdns;
pub mod packet;
pub mod qr;
pub mod spake2;

pub use client::{PairingOutcome, pair};
pub use error::Error;
pub use qr::QrSession;
