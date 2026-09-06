//! ADB device discovery, transport, and key management.
//!
//! Two transports are supported:
//!
//! 1. **USB transport** — talks to a device's `adbd` directly via the bulk endpoints
//!    (no `adb` binary required). Uses `rusb` (libusb) under the hood.
//! 2. **TCP transport** — talks to a device that has `adb pair`/`adb connect`'d,
//!    or an emulator. Standard adb-protocol-over-TCP.
//!
//! The transport layer is intentionally minimal: open a connection, send/receive
//! framed ADB messages (`OPEN`, `OKAY`, `CLSE`, `WRTE`, `SYNC` streams). Higher-level
//! concerns (file streaming, shell, proxy) live in other crates.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod auth;
pub mod connection;
pub mod device;
pub mod error;
pub mod packet;
pub mod transport;
pub mod watcher;

pub use auth::{generate_key, load_or_create_key, AdbKey};
pub use connection::{AdbConnection, StreamId};
pub use device::{DeviceId, DeviceInfo, DeviceState};
pub use error::{AdbError, Result};
pub use transport::{Transport, TransportKind, UsbTransport, TcpTransport};
pub use watcher::DeviceWatcher;
