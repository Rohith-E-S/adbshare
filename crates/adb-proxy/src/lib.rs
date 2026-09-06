//! High-throughput file transfer protocol over `adb forward`.
//!
//! ## Architecture
//!
//! 1. The host runs a one-time setup:
//!    - `adb push adbshare-proxy /data/local/tmp/adbshare-proxy`
//!    - `adb forward tcp:<port> tcp:<port>`
//!    - `adb shell exec /data/local/tmp/adbshare-proxy <port>` (lives until killed)
//! 2. The host opens one or more TCP connections to `127.0.0.1:<port>`.
//! 3. Each connection speaks a tiny length-prefixed binary RPC:
//!    ```text
//!    request:  u8 op | u32 len | payload bytes
//!    response: u32 len | u8 status | body bytes
//!    ```

#![warn(missing_debug_implementations)]

pub mod client;
pub mod ops;

#[cfg(test)]
mod tests;

pub use client::{ProxyClient, ProxyError, ProxyFile};
pub use ops::{FileMode, OpenFlags, Stat, Status};

/// The user-friendly name of the proxy binary on the device.
pub const PROXY_BIN_PATH: &str = "/data/local/tmp/adbshare-proxy";

/// Magic port we use for proxy-over-forward.
pub const DEFAULT_PROXY_PORT: u16 = 31337;

/// Size of the read/write buffer per file in the streaming read/write API.
pub const STREAM_CHUNK: usize = 256 * 1024;
