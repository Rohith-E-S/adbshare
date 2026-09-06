//! FUSE filesystem exposing an Android device (via the proxy) as a POSIX FS.

#![warn(missing_debug_implementations)]

pub mod cache;
pub mod filesystem;
pub mod mount;

pub use filesystem::Adbfs;

/// Run a FUSE mount with the given proxy client. Blocks the calling thread
/// until the mount is unmounted. Call this on a dedicated std::thread.
pub fn run(device: adb_device::DeviceId, client: adb_proxy::ProxyClient, mountpoint: std::path::PathBuf) -> Result<(), filesystem::FsError> {
    mount::run(device, client, mountpoint)
}
