//! Background daemon: discovers devices, mounts them, exposes D-Bus IPC.
//!
//! Lifecycle:
//! 1. Load or create the RSA auth key.
//! 2. Start the device watcher (subscribes to ADB-server events).
//! 3. For each new device, push the proxy binary, start the forwarding port,
//!    and connect a `ProxyClient` pool. Stash the client in shared state so
//!    the D-Bus interface and the transfer workers can both use it.
//! 4. Mount the FUSE filesystem on `$XDG_RUNTIME_DIR/adbshare/<serial>/`.
//! 5. Run a worker loop that drains the `JobQueue` and dispatches jobs to
//!    the matching device's `ProxyClient`.
//! 6. Expose `org.adbshare.Manager` D-Bus interface for the GUI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use adb_device::{DeviceId, DeviceWatcher};
use adb_proxy::{ops::DirEntry, ProxyClient, DEFAULT_PROXY_PORT, PROXY_BIN_PATH};
use adbfs;
use clap::Parser;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{error, info, warn};
use zbus::{interface, ConnectionBuilder};

use transfer_engine::{Direction, Job, JobOptions, JobQueue, Worker};

#[derive(Parser, Debug)]
#[command(name = "adb-daemon", version, about = "Background service for adbshare")]
struct Cli {
    /// Mount base directory.
    #[arg(long, env = "ADBSHARE_MOUNT_BASE")]
    mount_base: Option<PathBuf>,

    /// ADB server host:port.
    #[arg(long, default_value = "127.0.0.1:5037")]
    adb_server: String,

    /// Number of concurrent proxy connections per device.
    #[arg(long, default_value_t = 4)]
    proxy_conns: usize,

    /// Disable the FUSE mount (useful when running without fuse3 / in containers).
    #[arg(long)]
    no_fuse: bool,
}

#[derive(Debug)]
struct DeviceSlot {
    /// Mountpoint path (FUSE); None if `--no-fuse`.
    mountpoint: Option<PathBuf>,
    /// Pooled client to the device-side proxy binary.
    client: Arc<ProxyClient>,
    /// Host-side TCP port of this device's `adb forward`. Each device gets its
    /// own port so several devices can be connected simultaneously; the
    /// device-side port stays fixed at `DEFAULT_PROXY_PORT`.
    host_port: u16,
    /// Whether a full setup (push, forward, proxy start) completed for this
    /// device. Re-add/Changed events use it to decide between a health check
    /// and a full (proxy-killing) re-setup.
    setup_ok: bool,
}

#[derive(Debug, Default)]
struct State {
    /// serial -> slot
    devices: HashMap<DeviceId, DeviceSlot>,
}

/// Mountpoint for a device under `mount_base`, or None if the serial can't be
/// safely used as a path component (or FUSE is disabled).
fn mountpoint_for(mount_base: &std::path::Path, serial: &str, no_fuse: bool) -> Option<PathBuf> {
    if no_fuse {
        return None;
    }
    match sanitize_mount_name(serial) {
        Some(name) => Some(mount_base.join(name)),
        None => {
            warn!(serial, "serial is not usable as a mount directory; skipping FUSE mount");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_characters() {
        assert_eq!(sanitize_mount_name("abc-123_XY.09"), Some("abc-123_XY.09".into()));
    }

    #[test]
    fn sanitize_replaces_unsafe_characters() {
        assert_eq!(sanitize_mount_name("a/b\\c d"), Some("a_b_c_d".into()));
    }

    #[test]
    fn sanitize_rejects_dangerous_components() {
        assert_eq!(sanitize_mount_name(""), None);
        assert_eq!(sanitize_mount_name("."), None);
        assert_eq!(sanitize_mount_name(".."), None);
    }

    #[test]
    fn sanitize_cannot_escape_mount_base() {
        // A traversal attempt collapses to harmless underscores.
        let cleaned = sanitize_mount_name("../../etc").unwrap();
        assert!(!cleaned.contains('/'));
        assert_ne!(cleaned, "..");
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,adbfs=debug,transfer_engine=debug"))
        )
        .init();

    let cli = Cli::parse();
    let mount_base = cli.mount_base.clone().unwrap_or_else(|| {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/user/1000"));
        base.join("adbshare")
    });
    if !cli.no_fuse {
        std::fs::create_dir_all(&mount_base)?;
    }
    info!(?mount_base, no_fuse = cli.no_fuse, "mount base");

    let _key = adb_device::load_or_create_key()?;
    info!("auth key ready");

    let state = Arc::new(Mutex::new(State::default()));
    let (queue, mut queue_rx) = JobQueue::new(transfer_engine::DEFAULT_PARALLELISM);
    let watcher = DeviceWatcher::from_adb_server(
        &cli.adb_server.split(':').next().unwrap_or("127.0.0.1"),
        cli.adb_server.split(':').nth(1).and_then(|s| s.parse().ok()).unwrap_or(5037),
    );
    let mut events = watcher.subscribe();
    let state_clone = state.clone();
    let mount_base_clone = mount_base.clone();
    let proxy_conns = cli.proxy_conns;
    let no_fuse = cli.no_fuse;

    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                adb_device::watcher::WatchEvent::Added(id) => {
                    info!(?id, "device added");
                    ensure_device_ready(&state_clone, &id, &mount_base_clone, no_fuse, proxy_conns)
                        .await;
                }
                adb_device::watcher::WatchEvent::Removed(id) => {
                    info!(?id, "device removed");
                    let slot = state_clone.lock().devices.remove(&id);
                    if let Some(slot) = slot {
                        // Kill the on-device proxy and drop our forward
                        // (only ours — never the user's other forwards).
                        teardown_device(id.as_str(), slot.host_port).await;
                        if let Some(mp) = slot.mountpoint {
                            let mp_str = mp.to_string_lossy().into_owned();
                            let _ = Command::new("fusermount3").args(["-u", "-z", &mp_str]).status().await;
                        }
                    }
                }
                adb_device::watcher::WatchEvent::Changed(id) => {
                    info!(?id, "device state changed");
                    // If the device was added while still authorizing, setup
                    // failed; the user accepting the prompt fires this event —
                    // ensure_device_ready retries, but only tears down and
                    // re-runs setup when the existing setup is actually broken
                    // (a healthy setup is just health-checked, so active
                    // transfers aren't killed by a re-add/Changed event).
                    ensure_device_ready(&state_clone, &id, &mount_base_clone, no_fuse, proxy_conns)
                        .await;
                }
            }
        }
    });

    // Worker loop: drain pending jobs and dispatch to the right device's
    // ProxyClient via the `Job::device` tag.
    let state_for_workers = state.clone();
    let queue_for_workers = queue.clone();
    tokio::spawn(async move {
        while queue_rx.recv().await.is_some() {
            while let Some(job) = queue_for_workers.try_dispatch() {
                let serial = job.device.clone().unwrap_or_default();
                let client = {
                    let s = state_for_workers.lock();
                    s.devices.get(&DeviceId(serial.clone())).map(|slot| slot.client.clone())
                };
                let Some(client) = client else {
                    warn!(?serial, "no client for device; failing job");
                    job.set_state(transfer_engine::JobState::Failed);
                    job.set_error("device not connected");
                    queue_for_workers.mark_done(job);
                    continue;
                };
                let queue_inner = queue_for_workers.clone();
                tokio::spawn(async move {
                    let worker = Worker::new((*client).clone());
                    let result = Arc::new(worker).run(job.clone()).await;
                    if let Err(e) = result {
                        warn!(?e, id = job.id, "job failed");
                    }
                    queue_inner.mark_done(job);
                });
            }
        }
    });

    // D-Bus IPC.
    let conn = ConnectionBuilder::session()?
        .name("org.adbshare.Manager")?
        .serve_at("/org/adbshare/Manager", ManagerInterface {
            state: state.clone(),
            queue: queue.clone(),
        })?
        .build()
        .await?;
    info!("D-Bus service ready on org.adbshare.Manager");

    // Idle loop.
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    for (id, slot) in state.lock().devices.drain().collect::<Vec<_>>() {
        // Kill the on-device proxy and drop our host-side forward.
        teardown_device(id.as_str(), slot.host_port).await;
        if let Some(mp) = slot.mountpoint {
            let mp_str = mp.to_string_lossy().into_owned();
            let _ = Command::new("fusermount3").args(["-u", "-z", &mp_str]).status().await;
        }
    }
    drop(conn);
    Ok(())
}

/// Timeout for a single file-transfer-sized adb command (e.g. pushing the
/// proxy binary to the device).
const ADB_PUSH_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for quick adb commands (shell one-liners, forward, getprop).
const ADB_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `adb <args>` with a hard timeout so a hung device or adb server can't
/// stall the single watcher task. Returns the command's exit status.
async fn adb_run(args: &[&str], timeout: Duration) -> anyhow::Result<std::process::ExitStatus> {
    tokio::time::timeout(timeout, Command::new("adb").args(args).kill_on_drop(true).status())
        .await
        .map_err(|_| anyhow::anyhow!("adb {args:?} timed out"))?
        .map_err(|e| anyhow::anyhow!("adb {args:?}: {e}"))
}

/// `adb_run` variant that captures stdout/stderr.
async fn adb_run_output(args: &[&str], timeout: Duration) -> anyhow::Result<std::process::Output> {
    tokio::time::timeout(timeout, Command::new("adb").args(args).kill_on_drop(true).output())
        .await
        .map_err(|_| anyhow::anyhow!("adb {args:?} timed out"))?
        .map_err(|e| anyhow::anyhow!("adb {args:?}: {e}"))
}

/// True if the pooled client can still serve requests (proxy reachable).
async fn device_healthy(client: &ProxyClient) -> bool {
    tokio::time::timeout(Duration::from_secs(3), client.stat("/"))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Handle a device appearing (Added) or changing state (Changed).
///
/// If a setup already exists for this device we must NOT blindly re-run
/// `setup()`: it `pkill`s the on-device proxy, killing any in-flight
/// transfers. Instead, an existing setup is health-checked and only torn
/// down + re-set-up when it is actually broken (or `setup()` never
/// completed, in which case there is no slot at all).
///
/// NOTE: there is still a small race window between the health check and a
/// concurrent transfer, and a full re-setup does not stop the previous FUSE
/// thread; a complete device lifecycle rework is tracked separately.
async fn ensure_device_ready(
    state: &Arc<Mutex<State>>,
    id: &DeviceId,
    mount_base: &std::path::Path,
    no_fuse: bool,
    proxy_conns: usize,
) {
    let existing = state
        .lock()
        .devices
        .get(id)
        .map(|slot| (slot.client.clone(), slot.setup_ok, slot.host_port));
    if let Some((client, setup_ok, host_port)) = existing {
        if setup_ok && device_healthy(&client).await {
            info!(?id, "existing setup healthy; skipping re-setup");
            return;
        }
        warn!(?id, "existing setup broken; tearing down before re-setup");
        teardown_device(id.as_str(), host_port).await;
        state.lock().devices.remove(id);
    }

    let mp = mountpoint_for(mount_base, id.as_str(), no_fuse);
    if let Some(ref p) = mp {
        if let Err(_e) = std::fs::create_dir_all(p) {
            // Probably a stale mount from a previous run — try to clear it.
            let p_str = p.to_string_lossy().into_owned();
            let _ = Command::new("fusermount3").args(["-u", "-z", &p_str]).status().await;
            let _ = Command::new("umount").args(["-l", &p_str]).status().await;
            if let Err(e2) = std::fs::create_dir_all(p) {
                error!(?e2, "create mountpoint");
                return;
            }
        }
    }
    match setup(id.clone(), mp.clone(), proxy_conns).await {
        Ok((client, host_port)) => {
            state.lock().devices.insert(
                id.clone(),
                DeviceSlot {
                    mountpoint: mp,
                    client: Arc::new(client),
                    host_port,
                    setup_ok: true,
                },
            );
            info!(?id, "device ready");
        }
        Err(e) => {
            error!(?id, ?e, "setup/mount");
        }
    }
}

async fn setup(
    device: DeviceId,
    mountpoint: Option<PathBuf>,
    proxy_conns: usize,
) -> anyhow::Result<(ProxyClient, u16)> {
    let proxy_src = locate_proxy_binary(&device).await?;
    info!(?proxy_src, ?PROXY_BIN_PATH, "pushing proxy binary");
    // The device may still be waiting for the user to accept the USB
    // debugging prompt ("device still authorizing") — retry briefly. Each
    // push attempt is hard-timeboxed so one hung device can't stall the
    // watcher; transport-level errors (timeouts) give up after 3 attempts.
    let proxy_src_str = proxy_src.to_string_lossy().into_owned();
    let mut push_status = None;
    let mut push_err: Option<String> = None;
    for attempt in 0..15 {
        match adb_run(
            &["-s", device.as_str(), "push", &proxy_src_str, PROXY_BIN_PATH],
            ADB_PUSH_TIMEOUT,
        )
        .await
        {
            Ok(st) if st.success() => {
                push_status = Some(st);
                break;
            }
            Ok(st) => {
                // Usually "device still authorizing" — keep retrying.
                push_status = Some(st);
            }
            Err(e) => {
                push_err = Some(e.to_string());
                if attempt >= 2 {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    if let Some(e) = push_err {
        anyhow::bail!("adb push failed: {e}");
    }
    let status = push_status.expect("push loop set a status or bailed");
    if !status.success() {
        anyhow::bail!("adb push failed: {status}");
    }
    if let Err(e) = adb_run(
        &["-s", device.as_str(), "shell", "chmod", "755", PROXY_BIN_PATH],
        ADB_CMD_TIMEOUT,
    )
    .await
    {
        warn!(?e, "chmod on device failed (continuing)");
    }

    // Each device gets its own host-side port (a second device reusing the
    // same host port would fail `adb forward` with "address already in use",
    // and could misroute on older adb). The device side stays on
    // DEFAULT_PROXY_PORT; only the host half of the forward varies.
    let host_port = allocate_host_port()?;
    let status = adb_run(
        &[
            "-s",
            device.as_str(),
            "forward",
            &format!("tcp:{host_port}"),
            &format!("tcp:{DEFAULT_PROXY_PORT}"),
        ],
        ADB_CMD_TIMEOUT,
    )
    .await?;
    if !status.success() {
        anyhow::bail!("adb forward failed: {status}");
    }

    let _ = adb_run(
        &["-s", device.as_str(), "shell", "pkill", "-f", PROXY_BIN_PATH],
        ADB_CMD_TIMEOUT,
    )
    .await
    .ok();

    let proxy_cmd = format!(
        "setsid sh -c '{} {} >/data/local/tmp/adbshare-proxy.log 2>&1 &' </dev/null",
        PROXY_BIN_PATH, DEFAULT_PROXY_PORT
    );
    // Stdio is nulled on the host side; the proxy's output is redirected to a
    // log file on the device itself.
    let launch = tokio::time::timeout(
        ADB_CMD_TIMEOUT,
        Command::new("adb")
            .args(["-s", device.as_str(), "shell", &proxy_cmd])
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    if let Err(_) = launch {
        warn!("proxy launch timed out (continuing; health check will decide)");
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    let addr = format!("127.0.0.1:{host_port}");
    let mut last_err: Option<String> = None;
    for _ in 0..25 {
        let attempt_err: Option<String>;
        match tokio::time::timeout(Duration::from_secs(3), ProxyClient::connect(&addr, 1)).await {
            Ok(Ok(c)) => match tokio::time::timeout(Duration::from_secs(3), c.stat("/")).await {
                Ok(Ok(_)) => {
                    attempt_err = None;
                }
                Ok(Err(e)) => attempt_err = Some(e.to_string()),
                Err(_) => attempt_err = Some("proxy stat timed out".into()),
            },
            Ok(Err(e)) => attempt_err = Some(e.to_string()),
            Err(_) => attempt_err = Some("proxy connect timed out".into()),
        }
        match attempt_err {
            None => {
                last_err = None;
                break;
            }
            Some(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    if let Some(e) = last_err {
        let log = adb_run_output(
            &["-s", device.as_str(), "shell", "cat", "/data/local/tmp/adbshare-proxy.log"],
            ADB_CMD_TIMEOUT,
        )
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
        anyhow::bail!("proxy never came up: {e}. Device log: {log}");
    }

    let client = tokio::time::timeout(Duration::from_secs(5), ProxyClient::connect(&addr, proxy_conns))
        .await
        .map_err(|_| anyhow::anyhow!("proxy connect timed out"))??;

    if let Some(mp) = mountpoint {
        let device_for_thread = device.clone();
        let client_for_thread = client.clone();
        std::thread::Builder::new()
            .name(format!("adbfs-{}", device.as_str()))
            .spawn(move || {
                if let Err(e) = adbfs::run(device_for_thread, client_for_thread, mp) {
                    error!(?e, "fuse mount failed");
                }
            })?;
    }
    Ok((client, host_port))
}

/// Grab a free host TCP port by binding an ephemeral listener on 127.0.0.1
/// and immediately dropping it. There is a small TOCTOU window (another
/// process could claim the port before adb binds it); for dev tooling that
/// risk is acceptable and a collision surfaces as a loud `adb forward`
/// failure rather than silent misrouting.
fn allocate_host_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn is_x86_binary(path: &std::path::Path) -> bool {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() >= 20 && &bytes[0..4] == b"\x7fELF" {
            let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
            return machine == 0x3E || machine == 0x03;
        }
    }
    false
}

async fn locate_proxy_binary(device: &DeviceId) -> anyhow::Result<PathBuf> {
    if let Some(env_path) = std::env::var_os("ADBSHARE_PROXY_BIN").map(PathBuf::from) {
        if env_path.exists() { return Ok(env_path); }
    }

    let abi_output = adb_run_output(
        &["-s", device.as_str(), "shell", "getprop", "ro.product.cpu.abi"],
        ADB_CMD_TIMEOUT,
    )
    .await
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .unwrap_or_default();
    let abi = abi_output.trim().to_string();
    let is_arm = abi.contains("arm") || abi.contains("aarch64");

    let exe = std::env::current_exe().unwrap_or_default();
    let exe_dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));

    let mut candidates = Vec::new();
    if is_arm {
        candidates.push(PathBuf::from("target/aarch64-unknown-linux-musl/release/adbshare-proxy"));
        candidates.push(PathBuf::from("target/aarch64-linux-android/release/adbshare-proxy"));
        candidates.push(exe_dir.join("../aarch64-unknown-linux-musl/release/adbshare-proxy"));
        candidates.push(exe_dir.join("../aarch64-linux-android/release/adbshare-proxy"));
    }

    candidates.push(PathBuf::from("/usr/lib/adbshare/adbshare-proxy"));
    candidates.push(PathBuf::from("/app/bin/adbshare-proxy"));
    candidates.push(exe_dir.join("adbshare-proxy"));
    candidates.push(PathBuf::from("target/release/adbshare-proxy"));
    candidates.push(PathBuf::from("target/debug/adbshare-proxy"));

    for c in candidates {
        if c.exists() {
            if is_arm && is_x86_binary(&c) {
                continue;
            }
            return Ok(c);
        }
    }

    if let Ok(p) = which_("adbshare-proxy") {
        if !(is_arm && is_x86_binary(&p)) {
            return Ok(p);
        }
    }

    anyhow::bail!("adbshare-proxy binary not found for device ABI '{abi}'. Build with `cargo build --release --target aarch64-unknown-linux-musl --bin adbshare-proxy` or set ADBSHARE_PROXY_BIN.")
}

/// Best-effort cleanup for a device that is going away (or being torn down):
/// kill the on-device proxy process and remove our host-side adb forward.
/// Safe to call more than once; failures are logged, never fatal (the device
/// may already be unplugged).
async fn teardown_device(serial: &str, host_port: u16) {
    match adb_shell(serial, &format!("pkill -f {}", PROXY_BIN_PATH)).await {
        Ok(_) => {}
        // pkill exits non-zero when no process matched — that's fine.
        Err(e) => tracing::debug!(serial, host_port, %e, "pkill proxy (best-effort) failed"),
    }
    if let Err(e) = adb_run(
        &["-s", serial, "forward", "--remove", &format!("tcp:{host_port}")],
        ADB_CMD_TIMEOUT,
    )
    .await
    {
        tracing::debug!(serial, host_port, %e, "forward --remove (best-effort) failed");
    }
}

/// Sanitize a device serial for use as a mountpoint path component. A crafted
/// USB serial could contain `/` or `..` and escape the mount base directory.
/// Keep alphanumerics, `-`, `_`, `.`; replace everything else with `_`; reject
/// names that would be dangerous path components. The raw serial is still
/// used for `adb -s`.
fn sanitize_mount_name(serial: &str) -> Option<String> {
    let cleaned: String = serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned)
}

/// Serial looks like host:port (wireless pairing) vs a plain USB serial.
fn is_wireless_serial(serial: &str) -> bool {
    match serial.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

/// Run `adb -s <serial> shell <cmd>` with a hard timeout so an unresponsive
/// device can't stall the D-Bus call.
async fn adb_shell(serial: &str, cmd: &str) -> anyhow::Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("adb")
            .args(["-s", serial, "shell", cmd])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("adb shell timed out"))??;
    if !out.status.success() {
        anyhow::bail!("adb shell '{cmd}' failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `getprop` output ("[prop]: [value]" lines) into a map.
fn parse_getprop(raw: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[').and_then(|l| l.split_once("]: [")) {
            let value = rest.1.strip_suffix(']').unwrap_or(rest.1);
            map.insert(rest.0.to_string(), value.to_string());
        }
    }
    map
}

#[derive(Debug, Clone, Serialize)]
struct DeviceInfoDto {
    serial: String,
    model: Option<String>,
    android_version: Option<String>,
    /// "usb" or "wifi", inferred from the serial format.
    transport: &'static str,
    battery_pct: Option<u8>,
    storage_used: Option<u64>,
    storage_total: Option<u64>,
}

async fn device_info_json(serial: &str) -> anyhow::Result<String> {
    let transport = if is_wireless_serial(serial) { "wifi" } else { "usb" };

    let (props_raw, battery_raw, df_raw) = tokio::join!(
        adb_shell(serial, "getprop"),
        adb_shell(serial, "dumpsys battery"),
        adb_shell(serial, "df -k /sdcard"),
    );

    let (model, android_version) = match props_raw {
        Ok(raw) => {
            let props = parse_getprop(&raw);
            let model = props
                .get("ro.product.marketname")
                .filter(|s| !s.is_empty() && s.as_str() != "UNRECOGNIZED")
                .or_else(|| props.get("ro.product.model"))
                .filter(|s| !s.is_empty())
                .cloned();
            (
                model,
                props.get("ro.build.version.release").filter(|s| !s.is_empty()).cloned(),
            )
        }
        Err(e) => {
            warn!(serial, error = %e, "getprop failed");
            (None, None)
        }
    };

    let battery_pct = battery_raw.ok().and_then(|raw| {
        for line in raw.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("level:").and_then(|v| v.trim().parse::<u8>().ok()) {
                return Some(v.min(100));
            }
        }
        None
    });

    // toybox `df -k` last line: Filesystem 1K-blocks Used Available Use% Mounted
    let (storage_used, storage_total) = match df_raw {
        Ok(raw) => match raw.lines().filter(|l| !l.trim().is_empty()).last() {
            Some(line) => {
                let cols: Vec<&str> = line.split_whitespace().collect();
                if cols.len() >= 3 {
                    let total = cols[1].parse::<u64>().ok().map(|k| k * 1024);
                    let used = cols[2].parse::<u64>().ok().map(|k| k * 1024);
                    match (used, total) {
                        (Some(u), Some(t)) => (Some(u), Some(t)),
                        _ => (None, None),
                    }
                } else {
                    (None, None)
                }
            }
            None => (None, None),
        },
        Err(e) => {
            warn!(serial, error = %e, "df failed");
            (None, None)
        }
    };

    serde_json::to_string(&DeviceInfoDto {
        serial: serial.to_string(),
        model,
        android_version,
        transport,
        battery_pct,
        storage_used,
        storage_total,
    })
    .map_err(|e| anyhow::anyhow!("serialize device info: {e}"))
}

/// "Android Debug Bridge version 1.0.41" -> "1.0.41"
async fn adb_version() -> anyhow::Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new("adb").arg("version").kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("adb version timed out"))??;
    if !out.status.success() {
        anyhow::bail!("adb version failed");
    }
    let first = String::from_utf8_lossy(&out.stdout);
    let ver = first
        .lines()
        .next()
        .and_then(|l| l.rsplit(' ').next())
        .unwrap_or("unknown")
        .trim()
        .to_string();
    Ok(ver)
}

fn client_for(state: &Arc<Mutex<State>>, device: &str) -> Result<Arc<ProxyClient>, zbus::fdo::Error> {
    state.lock()
        .devices.get(&DeviceId(device.to_string()))
        .map(|slot| slot.client.clone())
        .ok_or_else(|| zbus::fdo::Error::ServiceUnknown("device not connected".into()))
}

/// Recursively delete `path` on the device via proxy ops (there is no
/// server-side `rm -r` in the proxy protocol).
async fn delete_recursive(client: &ProxyClient, path: &str) -> anyhow::Result<()> {
    let st = client.lstat(path).await?;
    if st.mode.is_symlink() || !st.mode.is_dir() {
        return client.unlink(path).await.map_err(|e| anyhow::anyhow!("{e}"));
    }
    for entry in client.listdir(path).await? {
        let child = format!("{}/{}", path.trim_end_matches('/'), entry.name);
        Box::pin(delete_recursive(client, &child)).await?;
    }
    client.rmdir(path).await.map_err(|e| anyhow::anyhow!("{e}"))
}

struct ManagerInterface {
    state: Arc<Mutex<State>>,
    queue: Arc<JobQueue>,
}

#[interface(name = "org.adbshare.Manager")]
impl ManagerInterface {
    /// List all devices currently set up (mounted + proxy connected).
    async fn list_devices(&self) -> zbus::fdo::Result<Vec<String>> {
        let s = self.state.lock();
        Ok(s.devices.keys().map(|d| d.to_string()).collect())
    }

    /// Live device metadata (model, transport, battery, storage) gathered
    /// over `adb shell`. Returns a JSON `DeviceInfoDto`.
    async fn device_info(&self, serial: String) -> zbus::fdo::Result<String> {
        device_info_json(&serial)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("device_info: {e}")))
    }

    /// Version string of the host `adb` server binary (e.g. "1.0.41").
    async fn adb_version(&self) -> zbus::fdo::Result<String> {
        adb_version()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("adb_version: {e}")))
    }

    /// FUSE mountpoint for a device, if mounted.
    async fn mountpoint_for(&self, device: &str) -> zbus::fdo::Result<String> {
        let s = self.state.lock();
        s.devices.get(&DeviceId(device.to_string()))
            .and_then(|slot| slot.mountpoint.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| zbus::fdo::Error::ServiceUnknown("device not mounted".into()))
    }

    async fn version(&self) -> zbus::fdo::Result<String> { Ok(env!("CARGO_PKG_VERSION").into()) }

    /// List a directory on a device. Returns a JSON array of entries so we
    /// don't have to plumb zvariant types through.
    async fn list_dir(&self, device: &str, path: &str) -> zbus::fdo::Result<String> {
        tracing::info!(%device, %path, "list_dir called");
        let client = {
            let s = self.state.lock();
            s.devices.get(&DeviceId(device.to_string())).map(|slot| slot.client.clone())
                .ok_or_else(|| zbus::fdo::Error::ServiceUnknown("device not connected".into()))?
        };
        let entries = client.listdir(path).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("listdir: {e}")))?;
        tracing::info!(%device, %path, n = entries.len(), "list_dir done");
        let mut dtos: Vec<DirEntryDto> = Vec::with_capacity(entries.len());
        for e in entries {
            let mut dto = DirEntryDto::from(e);
            if dto.is_symlink && !dto.is_dir {
                let full = if path == "/" {
                    format!("/{}", dto.name)
                } else {
                    format!("{}/{}", path.trim_end_matches('/'), dto.name)
                };
                if let Ok(st) = client.stat(&full).await {
                    if st.mode.is_dir() {
                        dto.is_dir = true;
                    }
                }
            }
            dtos.push(dto);
        }
        serde_json::to_string(&dtos)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    /// Enqueue a push (local file -> device). `local_path` is on the host;
    /// `device_path` is the absolute path on the phone. Returns the new
    /// job id.
    async fn enqueue_push(
        &self,
        device: &str,
        local_path: &str,
        device_path: &str,
    ) -> zbus::fdo::Result<u64> {
        let job = Job::with_device(
            0,
            Direction::Push,
            PathBuf::from(local_path),
            PathBuf::from(device_path),
            JobOptions::default(),
            Some(device.to_string()),
        );
        Ok(self.queue.submit(job))
    }

    /// Enqueue a pull (device -> local file). `device_path` is the absolute
    /// path on the phone; `local_path` is where it lands on the host.
    async fn enqueue_pull(
        &self,
        device: &str,
        device_path: &str,
        local_path: &str,
    ) -> zbus::fdo::Result<u64> {
        let job = Job::with_device(
            0,
            Direction::Pull,
            PathBuf::from(device_path),
            PathBuf::from(local_path),
            JobOptions::default(),
            Some(device.to_string()),
        );
        Ok(self.queue.submit(job))
    }

    /// Snapshot of every job currently tracked by the queue (JSON).
    async fn list_jobs(&self) -> zbus::fdo::Result<String> {
        let dtos: Vec<JobDto> = self.queue.jobs_snapshot().into_iter().map(JobDto::from).collect();
        serde_json::to_string(&dtos)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    // --- Transfer controls (banner Pause / Resume / Cancel in the GUI) ---

    async fn pause_job(&self, id: u64) -> zbus::fdo::Result<bool> {
        Ok(self.queue.pause_job(id))
    }

    async fn resume_job(&self, id: u64) -> zbus::fdo::Result<bool> {
        Ok(self.queue.resume_job(id))
    }

    async fn cancel_job(&self, id: u64) -> zbus::fdo::Result<bool> {
        Ok(self.queue.cancel_job(id))
    }

    // --- File operations on a device (via the on-device proxy) ---

    async fn mkdir(&self, device: &str, path: &str) -> zbus::fdo::Result<()> {
        let client = client_for(&self.state, device)?;
        client.mkdir(path, 0o755).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("mkdir: {e}")))
    }

    async fn rename(&self, device: &str, src: &str, dst: &str) -> zbus::fdo::Result<()> {
        let client = client_for(&self.state, device)?;
        client.rename(src, dst).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("rename: {e}")))
    }

    /// Delete a file or directory tree on the device.
    async fn delete(&self, device: &str, path: &str) -> zbus::fdo::Result<()> {
        let client = client_for(&self.state, device)?;
        delete_recursive(&client, path).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("delete: {e}")))
    }

    /// `adb connect <address>` — returns the adb CLI output so the GUI can
    /// surface it. The address must look like host[:port].
    async fn connect_wireless(&self, address: &str) -> zbus::fdo::Result<String> {
        let ok = !address.is_empty()
            && !address.contains([';', '&', '|', '$', '`', '\n', ' '])
            && address.rsplit_once(':').map(|(_, p)| p.parse::<u16>().is_ok()).unwrap_or(false);
        if !ok {
            return Err(zbus::fdo::Error::InvalidArgs("address must be host:port".into()));
        }
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("adb")
                .args(["connect", address])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| zbus::fdo::Error::Failed(format!("adb connect {address} timed out")))?
        .map_err(|e| zbus::fdo::Error::Failed(format!("adb connect: {e}")))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        if !out.status.success() {
            return Err(zbus::fdo::Error::Failed(text));
        }
        Ok(text)
    }
}

#[allow(dead_code)]
fn which_(name: &str) -> std::result::Result<PathBuf, std::io::Error> {
    let path = std::env::var_os("PATH").ok_or_else(|| std::io::Error::other("no PATH"))?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() { return Ok(p); }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DirEntryDto {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mode: u32,
    mtime: i64,
}

impl From<DirEntry> for DirEntryDto {
    fn from(e: DirEntry) -> Self {
        Self {
            name: e.name,
            is_dir: e.stat.mode.is_dir(),
            is_symlink: e.stat.mode.is_symlink(),
            size: e.stat.size,
            mode: e.stat.mode.0,
            mtime: e.stat.mtime,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobDto {
    id: u64,
    direction: Direction,
    source: String,
    destination: String,
    state: transfer_engine::JobState,
    bytes_done: u64,
    bytes_total: u64,
    speed_bps: u64,
    eta_secs: u64,
}

impl From<Job> for JobDto {
    fn from(j: Job) -> Self {
        Self {
            id: j.id,
            direction: j.direction,
            source: j.source.to_string_lossy().into_owned(),
            destination: j.destination.to_string_lossy().into_owned(),
            state: j.state(),
            bytes_done: j.bytes_done(),
            bytes_total: j.bytes_total(),
            speed_bps: j.speed_bps(),
            eta_secs: j.eta_secs(),
        }
    }
}