//! Watches for device connect/disconnect events.
//!
//! Two implementations:
//! - `NetstatWatcher` — connects to the ADB server (port 5037) and issues
//!   `host:track-devices` to receive a live list. This is what `adb` itself
//!   does. Works for all transports.
//! - `UsbWatcher` (Linux) — uses `rusb` to enumerate USB devices. Useful as a
//!   fallback when there's no ADB server running.

use std::sync::Arc;

use parking_lot::Mutex as PlMutex;
use tokio::sync::mpsc;

use crate::device::{DeviceId, DeviceInfo, DeviceState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Added(DeviceId),
    Removed(DeviceId),
    Changed(DeviceId),
}

#[derive(Debug)]
pub struct DeviceWatcher {
    inner: Arc<Mutex<dyn WatcherImpl + Send>>,
}

mod inner {
    use super::*;
    pub trait WatcherImpl: std::fmt::Debug + Send {
        fn poll(&self) -> Vec<WatchEvent>;
    }
}
use inner::WatcherImpl;

impl Clone for DeviceWatcher {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl DeviceWatcher {
    /// Watch by talking to a running `adb server` at `host:port` (default 127.0.0.1:5037).
    pub fn from_adb_server(host: &str, port: u16) -> Self {
        let host = host.to_string();
        Self {
            inner: Arc::new(Mutex::new(NetstatWatcher::new(host, port))),
        }
    }

    /// Watch by enumerating USB devices directly via libusb.
    pub fn from_usb() -> Self {
        Self { inner: Arc::new(Mutex::new(UsbWatcher::new())) }
    }

    /// Subscribe to device events. Returns a receiver and starts a background task
    /// that polls the underlying watcher at ~1 Hz.
    pub fn subscribe(&self) -> mpsc::Receiver<WatchEvent> {
        let (tx, rx) = mpsc::channel::<WatchEvent>(64);
        let inner = self.inner.clone();
        // The watcher must drive its own runtime so the underlying poll can
        // do sync I/O (e.g. via tokio::task::spawn_blocking).
        std::thread::Builder::new()
            .name("adb-watcher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build watcher runtime");
                rt.block_on(async move {
                    let mut last: Vec<DeviceInfo> = Vec::new();
                    loop {
                        // Spawn the poll on a blocking task so we can use
                        // sync I/O without blocking the runtime.
                        let inner = inner.clone();
                        let events = tokio::task::spawn_blocking(move || {
                            match inner.lock() {
                                Ok(g) => g.poll(),
                                Err(_) => Vec::new(),
                            }
                        })
                            .await
                            .unwrap_or_default();
                        if !events.is_empty() {
                            tracing::info!(?events, "device events");
                        }
                        for ev in events {
                            if tx.send(ev).await.is_err() {
                                return;
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                        let _ = &mut last;
                    }
                });
            })
            .expect("spawn watcher thread");
        rx
    }
}

use std::sync::Mutex;
use std::sync::MutexGuard;

// ---------- ADB-server watcher ----------

#[derive(Debug)]
struct NetstatWatcher {
    host: String,
    port: u16,
    state: PlMutex<NetstatState>,
}

#[derive(Debug, Default)]
struct NetstatState {
    last: Vec<DeviceInfo>,
}

impl NetstatWatcher {
    fn new(host: String, port: u16) -> Self {
        Self { host, port, state: PlMutex::new(NetstatState::default()) }
    }

    fn fetch(&self) -> std::io::Result<Vec<DeviceInfo>> {
        // Run the async query on a fresh current-thread runtime. This
        // poll() is called from a blocking task, so we own this thread
        // for the duration.
        let host = self.host.clone();
        let port = self.port;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move { query_adb_server(&host, port).await })
    }
}

impl WatcherImpl for NetstatWatcher {
    fn poll(&self) -> Vec<WatchEvent> {
        // On a transient query failure (adb server restarting, busy, ...)
        // keep the previous device set instead of treating it as empty;
        // emitting Removed for every device and re-Adding them next poll
        // makes clients flap.
        let current = match self.fetch() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(?e, "adb-server query failed; keeping previous device set");
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        let mut state = self.state.lock();
        let prev: Vec<DeviceInfo> = std::mem::take(&mut state.last);

        let prev_ids: std::collections::HashSet<_> = prev.iter().map(|d| d.id.clone()).collect();
        let cur_ids: std::collections::HashSet<_> = current.iter().map(|d| d.id.clone()).collect();

        for d in &current {
            if !prev_ids.contains(&d.id) {
                events.push(WatchEvent::Added(d.id.clone()));
            } else if prev.iter().find(|p| p.id == d.id).map(|p| p.state != d.state).unwrap_or(false) {
                events.push(WatchEvent::Changed(d.id.clone()));
            }
        }
        for p in &prev {
            if !cur_ids.contains(&p.id) {
                events.push(WatchEvent::Removed(p.id.clone()));
            }
        }

        state.last = current;
        events
    }
}

async fn query_adb_server(host: &str, port: u16) -> std::io::Result<Vec<DeviceInfo>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    // Resolve the address and bound the connect attempt: without a timeout a
    // half-up adb server can stall the watcher thread indefinitely.
    let addr = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| std::io::Error::other(format!("resolve {host}:{port}: {e}")))?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("no address for {host}:{port}")))?;
    let mut stream = tokio::time::timeout(std::time::Duration::from_secs(1), TcpStream::connect(addr))
        .await
        .map_err(|_| std::io::Error::other(format!("connect to {host}:{port} timed out")))??;
    let payload = b"host:devices-l";
    let length_hex = format!("{:04x}", payload.len());
    stream.write_all(length_hex.as_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    // Read exactly 8 bytes: 4-byte status ("OKAY"/"FAIL") + 4-byte hex length.
    let mut header = [0u8; 8];
    reader.read_exact(&mut header).await?;
    let status = std::str::from_utf8(&header[0..4])
        .map_err(|e| std::io::Error::other(format!("bad status: {e}")))?;
    if status != "OKAY" {
        return Err(std::io::Error::other(format!("adb server: {status}")));
    }
    let len_str = std::str::from_utf8(&header[4..8])
        .map_err(|e| std::io::Error::other(format!("bad length: {e}")))?;
    let body_len = usize::from_str_radix(len_str, 16)
        .map_err(|e| std::io::Error::other(format!("bad length '{len_str}': {e}")))?;
    let mut body = vec![0u8; body_len];
    reader.read_exact(&mut body).await?;
    let devices = String::from_utf8_lossy(&body).into_owned();
    let mut out = Vec::new();
    for line in devices.lines() {
        if line.is_empty() { continue; }
        let mut parts = line.split_whitespace();
        let serial = parts.next().unwrap_or("").to_string();
        let state = match parts.next().unwrap_or("") {
            "device" => DeviceState::Online,
            "offline" => DeviceState::Offline,
            "unauthorized" => DeviceState::Unauthorized,
            "recovery" => DeviceState::Recovery,
            "sideload" => DeviceState::Sideload,
            "bootloader" => DeviceState::Bootloader,
            _ => DeviceState::Unknown,
        };
        if serial.is_empty() { continue; }
        out.push(DeviceInfo {
            id: DeviceId(serial),
            state,
            model: None,
            product: None,
            device: None,
            transport_id: None,
        });
    }
    Ok(out)
}

// ---------- USB watcher (libusb) ----------

#[derive(Debug)]
struct UsbWatcher {
    state: PlMutex<Vec<DeviceInfo>>,
}

impl UsbWatcher {
    fn new() -> Self { Self { state: PlMutex::new(Vec::new()) } }
}

const VENDOR_ANDROID: u16 = 0x18d1;
const VENDOR_GOOGLE: u16 = 0x04e8;

impl WatcherImpl for UsbWatcher {
    fn poll(&self) -> Vec<WatchEvent> {
        use rusb::UsbContext;
        let Ok(context) = rusb::Context::new() else { return Vec::new() };
        let mut current = Vec::new();
        for device in context.devices().unwrap().iter() {
            let Ok(desc) = device.device_descriptor() else { continue; };
            if desc.vendor_id() != VENDOR_ANDROID && desc.vendor_id() != VENDOR_GOOGLE { continue; }
            let bus = device.bus_number();
            let addr = device.address();
            current.push(DeviceInfo {
                id: DeviceId(format!("usb:{:03}:{:03}", bus, addr)),
                state: DeviceState::Online,
                model: None,
                product: None,
                device: None,
                transport_id: None,
            });
        }
        let mut events = Vec::new();
        let mut state = self.state.lock();
        let prev: Vec<DeviceInfo> = std::mem::take(&mut state);
        let prev_ids: std::collections::HashSet<_> = prev.iter().map(|d| d.id.clone()).collect();
        let cur_ids: std::collections::HashSet<_> = current.iter().map(|d| d.id.clone()).collect();
        for d in &current {
            if !prev_ids.contains(&d.id) {
                events.push(WatchEvent::Added(d.id.clone()));
            }
        }
        for p in &prev {
            if !cur_ids.contains(&p.id) {
                events.push(WatchEvent::Removed(p.id.clone()));
            }
        }
        *state = current;
        events
    }
}

use tokio::net::TcpStream;

