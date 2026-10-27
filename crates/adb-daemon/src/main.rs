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
use std::time::{Duration, Instant};

use adb_device::{DeviceId, DeviceWatcher};
use adb_proxy::{DEFAULT_PROXY_PORT, PROXY_BIN_PATH, ProxyClient, ProxyError, ops::DirEntry};
use clap::Parser;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{debug, error, info, warn};
use zbus::{ConnectionBuilder, interface};

use transfer_engine::{Direction, Job, JobOptions, JobQueue, Worker};

mod packages;

#[derive(Parser, Debug)]
#[command(
    name = "adb-daemon",
    version,
    about = "Background service for adbshare"
)]
struct Cli {
    /// Mount base directory.
    #[arg(long, env = "ADBSHARE_MOUNT_BASE")]
    mount_base: Option<PathBuf>,

    /// ADB server host:port.
    #[arg(long, default_value = "127.0.0.1:5037")]
    adb_server: String,

    /// Number of concurrent proxy connections per device (1-64).
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
}

/// One running (or just-finished) package-install operation.
///
/// Installs deliberately sit beside the transfer queue rather than inside it:
/// an install is `adb push` + `pm install`, not a proxy op, and the GUI wants
/// per-stage status — which the queue's byte counters cannot express. The GUI
/// reads these through `install_status_all` on its regular job poll.
#[derive(Debug, Clone, Serialize)]
struct InstallStatus {
    id: u64,
    device: String,
    /// File name shown to the user while the source is out of sight.
    label: String,
    source: String,
    /// True when the APK already lived on the phone, so there is no transfer.
    from_device: bool,
    /// `false` before the first progress reading arrives.
    stage: u32,
    percent: u8,
    bytes_done: u64,
    bytes_total: u64,
    /// `Pending` | `Copying` | `Installing` | `Done` | `Failed`.
    state: String,
    /// Code (`downgrade`, `signature_mismatch`, …) so the GUI can offer the
    /// matching recovery instead of a raw string to stare at.
    error_code: Option<String>,
    message: Option<String>,
    raw: Option<String>,
    finished: bool,
}

#[derive(Debug, Default)]
struct State {
    /// serial -> slot
    devices: HashMap<DeviceId, DeviceSlot>,
    adb_server: String,
    mount_base: PathBuf,
    proxy_conns: usize,
    no_fuse: bool,
    /// Install operations the GUI can watch. Kept here rather than the queue
    /// for the reasons on `InstallStatus`.
    installs: HashMap<u64, InstallStatus>,
    /// True while a pipeline is actually executing, so a second request can be
    /// told "one install at a time" rather than overlap two `pm install` runs.
    install_running: bool,
    /// The id the next install gets. `pm` cannot run two session installs at
    /// once without running into stale-session errors, so installs run one at
    /// a time and the id doubles as an ordering guarantee.
    next_install_id: u64,
    /// Host RSA identity, loaded once at startup. The daemon drives every
    /// device through the `adb` binary, which does its own ADB auth from
    /// `~/.android/adbkey`, so this is not needed for the `adb` path — but it
    /// must be held rather than dropped, so an in-process transport
    /// (`TcpTransport`/`UsbTransport`) has a key to authenticate with.
    auth_key: Option<Arc<adb_device::AdbKey>>,
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
            warn!(
                serial,
                "serial is not usable as a mount directory; skipping FUSE mount"
            );
            None
        }
    }
}

/// This process's uid, without pulling in a dependency for it.
fn nix_uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_string))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

/// Parse an `--adb-server` "host:port" spec. Handles bracketed IPv6 hosts
/// like `[::1]:5037`, which a naive `split(':')` would shred. Falls back to
/// the adb default port 5037 when no (valid) port is present.
fn parse_adb_server(spec: &str) -> (String, u16) {
    if let Some(rest) = spec.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(5037);
        return (host.to_string(), port);
    }
    // Unbracketed with more than one colon: a bare IPv6 literal such as `::1`.
    // `rsplit_once` would split it at the last colon into ("::", "1"), and "1"
    // is a perfectly good port, so nothing downstream notices the mangling —
    // the watcher just listens on `tcp:::1:1`, never connects, and device
    // discovery silently produces no events.
    if !spec.starts_with('[') && spec.matches(':').count() > 1 {
        return (spec.to_string(), 5037);
    }
    match spec.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(5037)),
        None => (spec.to_string(), 5037),
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,adbfs=debug,transfer_engine=debug")
            }),
        )
        .init();

    let cli = Cli::parse();
    // `--proxy-conns 0` would create a zero-permit semaphore that deadlocks
    // every RPC; refuse it (and absurd values) with a clear error.
    if !(1..=64).contains(&cli.proxy_conns) {
        anyhow::bail!(
            "--proxy-conns must be between 1 and 64 (got {})",
            cli.proxy_conns
        );
    }
    let mount_base = cli.mount_base.clone().unwrap_or_else(|| {
        // Fall back to the *current* uid's runtime dir, not a hardcoded 1000:
        // with XDG_RUNTIME_DIR unset (a systemd unit that does not pass it, a
        // non-standard session) any user other than 1000 got "Permission
        // denied", and the `?` on create_dir_all below made main() return Err
        // — the daemon exited before ever registering on D-Bus.
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let uid = nix_uid();
                let dir = if uid == 0 {
                    std::env::temp_dir()
                } else {
                    PathBuf::from(format!("/run/user/{uid}"))
                };
                if std::fs::create_dir_all(&dir).is_err() {
                    // Last resort: a temp dir is always writable.
                    std::env::temp_dir()
                } else {
                    dir
                }
            });
        base.join("adbshare")
    });
    if !cli.no_fuse {
        std::fs::create_dir_all(&mount_base)?;
    }
    info!(?mount_base, no_fuse = cli.no_fuse, "mount base");

    let key = adb_device::load_or_create_key()?;
    info!(path = ?key.path(), "auth key ready");

    let state = Arc::new(Mutex::new(State {
        adb_server: cli.adb_server.clone(),
        mount_base: mount_base.clone(),
        proxy_conns: cli.proxy_conns,
        no_fuse: cli.no_fuse,
        auth_key: Some(Arc::new(key)),
        ..State::default()
    }));
    let (queue, mut queue_rx) = JobQueue::new(transfer_engine::DEFAULT_PARALLELISM);
    let (adb_host, adb_port) = parse_adb_server(&cli.adb_server);
    info!(host = %adb_host, port = adb_port, "adb server");
    if adb_host != "127.0.0.1" || adb_port != 5037 {
        // Must be set before anything spawns an `adb` process. `set` rather than
        // `get_or_init` so a second call is a loud bug, not a silent no-op.
        ADB_SERVER
            .set(Some((adb_host.clone(), adb_port)))
            .expect("ADB_SERVER set once during startup");
    }
    let watcher = DeviceWatcher::from_adb_server(&adb_host, adb_port);
    let mut events = watcher.subscribe();
    let state_clone = state.clone();
    let mount_base_clone = mount_base.clone();
    let proxy_conns = cli.proxy_conns;
    let no_fuse = cli.no_fuse;
    let queue_for_setup = queue.clone();

    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                adb_device::watcher::WatchEvent::Added(id) => {
                    info!(?id, "device added");
                    ensure_device_ready(
                        &state_clone,
                        &queue_for_setup,
                        &id,
                        &mount_base_clone,
                        no_fuse,
                        proxy_conns,
                    )
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
                            let _ = Command::new("fusermount3")
                                .args(["-u", "-z", &mp_str])
                                .status()
                                .await;
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
                    ensure_device_ready(
                        &state_clone,
                        &queue_for_setup,
                        &id,
                        &mount_base_clone,
                        no_fuse,
                        proxy_conns,
                    )
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
                    s.devices
                        .get(&DeviceId(serial.clone()))
                        .map(|slot| slot.client.clone())
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
                    // Drop-based, not a trailing statement: a panic inside
                    // `run` unwinds past a plain `mark_done` call and would
                    // strand the job in `in_flight` forever. `mark_done` is the
                    // only thing that removes it, and a stranded slot is
                    // unrecoverable — `retry_failed` only scans `completed`, so
                    // the job could not be retried, cancelled or trimmed. After
                    // `parallelism` such panics the engine stops dispatching
                    // altogether.
                    let _done = MarkDoneOnDrop {
                        queue: queue_inner,
                        job: job.clone(),
                    };
                    let worker = Worker::new((*client).clone());
                    let result = Arc::new(worker).run(job.clone()).await;
                    if let Err(e) = result {
                        warn!(?e, id = job.id, "job failed");
                    }
                });
            }
        }
    });

    // D-Bus IPC.
    let conn = ConnectionBuilder::session()?
        .name("org.adbshare.Manager")?
        .serve_at(
            "/org/adbshare/Manager",
            ManagerInterface {
                state: state.clone(),
                queue: queue.clone(),
                device_info: Arc::new(TimedCache::new(DEVICE_INFO_TTL)),
                package_info: Arc::new(TimedCache::new(PACKAGE_INFO_TTL)),
                packages: Arc::new(TimedCache::new(packages::INVENTORY_TTL)),
            },
        )?
        .build()
        .await?;
    info!("D-Bus service ready on org.adbshare.Manager");

    // Idle loop.
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    // Drain first: holding the state lock across `await` would block every
    // device_info reader for the length of the teardown.
    let devices = std::mem::take(&mut state.lock().devices);
    for (id, slot) in devices {
        // Kill the on-device proxy and drop our host-side forward.
        teardown_device(id.as_str(), slot.host_port).await;
        if let Some(mp) = slot.mountpoint {
            let mp_str = mp.to_string_lossy().into_owned();
            let _ = Command::new("fusermount3")
                .args(["-u", "-z", &mp_str])
                .status()
                .await;
        }
    }
    drop(conn);
    Ok(())
}

/// Timeout for a single file-transfer-sized adb command (e.g. pushing the
/// proxy binary to the device).
const ADB_PUSH_TIMEOUT: Duration = Duration::from_secs(30);

/// Budget for the liveness probe in `device_healthy`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a device metadata reading is reused before it is gathered again.
///
/// The GUI polls every three seconds; fifteen seconds turns five sets of three
/// `adb shell` subprocesses per minute into one, which is still far more often
/// than a battery percentage or a storage total meaningfully changes.
const DEVICE_INFO_TTL: Duration = Duration::from_secs(15);
/// How long a `package_info` reading may be reused before `dumpsys` is asked
/// again — a package version changes only on install, so this is deliberately
/// longer than the device sweep.
const PACKAGE_INFO_TTL: Duration = Duration::from_secs(30);
/// How long `pm install` may take to verify and commit a package. Large game
/// bundles take minutes; the transfer stage has its own budget.
const INSTALL_PM_TIMEOUT: Duration = Duration::from_secs(300);
/// How long `adb push` may take for one APK.
const INSTALL_PUSH_TIMEOUT: Duration = Duration::from_secs(600);
/// How long copying a device-local APK into the staging area may take.
const INSTALL_CP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `pm uninstall` may take.
const UNINSTALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Where an APK is parked on the device before `pm install` runs.
const INSTALL_STAGE: &str = "/data/local/tmp/adbshare_install_stage.apk";
/// How often the transfer stage re-reads the staged file's size, which is what
/// turns `adb push` into live byte progress.
const INSTALL_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How many finished installs are kept on the map, so a long session does not
/// grow it without bound while the GUI's list stays within reach.
const INSTALL_HISTORY_CAP: usize = 25;

/// Timeout for quick adb commands (shell one-liners, forward, getprop).
const ADB_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// How many packages one batch may change. A list of two thousand is a sign
/// something went wrong, not a request.
const MAX_PACKAGE_ACTIONS: usize = 1000;

/// Run `adb <args>` with a hard timeout so a hung device or adb server can't
/// stall the single watcher task. Returns the command's exit status.
/// The ADB server that every `adb` invocation must talk to, from
/// `--adb-server`. `None` means "the adb default", in which case no `-H`/`-P`
/// is passed at all.
///
/// Set once during startup, before any device work begins. The device watcher
/// was given the parsed host/port, but every actual `adb` call used a bare
/// `adb`, so a non-default server was watched and never used: `push` reported
/// "device not found" and no device ever came up.
static ADB_SERVER: std::sync::OnceLock<Option<(String, u16)>> = std::sync::OnceLock::new();

/// Build an `adb` command already pointed at the configured server.
///
/// Every adb invocation in this binary must be built here rather than with a
/// bare `Command::new("adb")`, which silently talks to 127.0.0.1:5037 and
/// ignores `--adb-server`.
fn adb_command() -> Command {
    let mut cmd = Command::new("adb");
    if let Some(Some((host, port))) = ADB_SERVER.get() {
        let port = port.to_string();
        cmd.arg("-H").arg(host).arg("-P").arg(port);
    }
    cmd
}

async fn adb_run(args: &[&str], timeout: Duration) -> anyhow::Result<std::process::ExitStatus> {
    tokio::time::timeout(
        timeout,
        adb_command().args(args).kill_on_drop(true).status(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("adb {args:?} timed out"))?
    .map_err(|e| anyhow::anyhow!("adb {args:?}: {e}"))
}

/// `adb_run` variant that captures stdout/stderr.
async fn adb_run_output(args: &[&str], timeout: Duration) -> anyhow::Result<std::process::Output> {
    tokio::time::timeout(
        timeout,
        adb_command().args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("adb {args:?} timed out"))?
    .map_err(|e| anyhow::anyhow!("adb {args:?}: {e}"))
}

/// Returns a dispatched job to the queue when the worker finishes — or
/// panics. Named fields so it cannot be constructed by accident.
struct MarkDoneOnDrop {
    queue: Arc<JobQueue>,
    job: Job,
}

impl Drop for MarkDoneOnDrop {
    fn drop(&mut self) {
        self.queue.mark_done(self.job.clone());
    }
}

/// True if the pooled client can still serve requests (proxy reachable).
///
/// A saturated connection pool is deliberately *not* treated as a dead
/// device. `--proxy-conns` defaults to `DEFAULT_PARALLELISM`, so a full batch
/// of transfers saturates the pool, and the probe then never reaches the
/// network. Reporting "unhealthy" there would tear down a working device and
/// kill every in-flight transfer with it.
async fn device_healthy(client: &ProxyClient) -> bool {
    match tokio::time::timeout(HEALTH_TIMEOUT, client.stat("/")).await {
        Ok(Ok(_)) => true,
        // Busy says nothing about the device; only a real failure does.
        Ok(Err(ProxyError::Busy)) => {
            debug!("health check: pool busy, treating device as healthy");
            true
        }
        Ok(Err(e)) => {
            warn!(error = ?e, "health check failed");
            false
        }
        Err(_) => false,
    }
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
    queue: &Arc<JobQueue>,
    id: &DeviceId,
    mount_base: &std::path::Path,
    no_fuse: bool,
    proxy_conns: usize,
) {
    let existing = state
        .lock()
        .devices
        .get(id)
        .map(|slot| (slot.client.clone(), slot.host_port));
    if let Some((client, host_port)) = existing {
        // A slot only exists once setup succeeded -- a failed setup inserts
        // nothing -- so reaching here means "already set up", and the only
        // question is whether the proxy is still healthy.
        if device_healthy(&client).await {
            info!(?id, "existing setup healthy; skipping re-setup");
            return;
        }
        warn!(?id, "existing setup broken; tearing down before re-setup");
        teardown_device(id.as_str(), host_port).await;
        state.lock().devices.remove(id);
    }

    let mp = mountpoint_for(mount_base, id.as_str(), no_fuse);
    if let Some(ref p) = mp
        && let Err(_e) = std::fs::create_dir_all(p)
    {
        // Probably a stale mount from a previous run — try to clear it.
        let p_str = p.to_string_lossy().into_owned();
        let _ = Command::new("fusermount3")
            .args(["-u", "-z", &p_str])
            .status()
            .await;
        let _ = Command::new("umount").args(["-l", &p_str]).status().await;
        if let Err(e2) = std::fs::create_dir_all(p) {
            error!(?e2, "create mountpoint");
            return;
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
                },
            );
            info!(?id, "device ready");
            // Replug: retry jobs that failed for this device while it was
            // away (auto-requeue on reconnect).
            let requeued = queue.retry_failed_for(id.as_str());
            if requeued > 0 {
                info!(?id, requeued, "requeued failed jobs after replug");
            }
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
            &[
                "-s",
                device.as_str(),
                "push",
                &proxy_src_str,
                PROXY_BIN_PATH,
            ],
            ADB_PUSH_TIMEOUT,
        )
        .await
        {
            Ok(st) if st.success() => {
                push_status = Some(st);
                // A transport error from an earlier attempt must not be
                // reported against this successful push: the loop can time
                // out while the device is still authorising and then succeed
                // on a later try.
                push_err = None;
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
        &[
            "-s",
            device.as_str(),
            "shell",
            "chmod",
            "755",
            PROXY_BIN_PATH,
        ],
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

    // From here the forward exists. Everything below can still fail, and a
    // failed setup inserts no DeviceSlot — so the `Removed` handler has
    // nothing to clean up with and the forward would outlive this attempt,
    // holding a host port bound by adb. Release it explicitly on failure.
    match finish_setup(device.clone(), mountpoint, proxy_conns, host_port).await {
        Ok(pair) => Ok(pair),
        Err(e) => {
            warn!(
                device = device.as_str(),
                host_port,
                error = ?e,
                "setup failed after the forward was created; releasing it"
            );
            teardown_device(device.as_str(), host_port).await;
            Err(e)
        }
    }
}

/// Everything in [`setup`] that happens after `adb forward` has succeeded.
async fn finish_setup(
    device: DeviceId,
    mountpoint: Option<PathBuf>,
    proxy_conns: usize,
    host_port: u16,
) -> anyhow::Result<(ProxyClient, u16)> {
    let addr = format!("127.0.0.1:{host_port}");

    let _ = adb_run(
        &[
            "-s",
            device.as_str(),
            "shell",
            "pkill",
            "-f",
            PROXY_BIN_PATH,
        ],
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
        adb_command()
            .args(["-s", device.as_str(), "shell", &proxy_cmd])
            .kill_on_drop(true)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    if launch.is_err() {
        warn!("proxy launch timed out (continuing; health check will decide)");
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

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
            &[
                "-s",
                device.as_str(),
                "shell",
                "cat",
                "/data/local/tmp/adbshare-proxy.log",
            ],
            ADB_CMD_TIMEOUT,
        )
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
        anyhow::bail!("proxy never came up: {e}. Device log: {log}");
    }

    let client = tokio::time::timeout(
        Duration::from_secs(5),
        ProxyClient::connect(&addr, proxy_conns),
    )
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
    if let Ok(bytes) = std::fs::read(path)
        && bytes.len() >= 20
        && &bytes[0..4] == b"\x7fELF"
    {
        let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
        return machine == 0x3E || machine == 0x03;
    }
    false
}

async fn locate_proxy_binary(device: &DeviceId) -> anyhow::Result<PathBuf> {
    if let Some(env_path) = std::env::var_os("ADBSHARE_PROXY_BIN").map(PathBuf::from)
        && env_path.exists()
    {
        return Ok(env_path);
    }

    let abi_output = adb_run_output(
        &[
            "-s",
            device.as_str(),
            "shell",
            "getprop",
            "ro.product.cpu.abi",
        ],
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
        candidates.push(PathBuf::from(
            "target/aarch64-unknown-linux-musl/release/adbshare-proxy",
        ));
        candidates.push(PathBuf::from(
            "target/aarch64-linux-android/release/adbshare-proxy",
        ));
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

    if let Ok(p) = which_("adbshare-proxy")
        && !(is_arm && is_x86_binary(&p))
    {
        return Ok(p);
    }

    anyhow::bail!(
        "adbshare-proxy binary not found for device ABI '{abi}'. Build with `cargo build --release --target aarch64-unknown-linux-musl --bin adbshare-proxy` or set ADBSHARE_PROXY_BIN."
    )
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
        &[
            "-s",
            serial,
            "forward",
            "--remove",
            &format!("tcp:{host_port}"),
        ],
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

/// Run `adb -s <serial> shell <cmd>` with a caller-chosen timeout so an
/// unresponsive device can't stall the D-Bus call.
async fn adb_shell_with(serial: &str, cmd: &str, timeout: Duration) -> anyhow::Result<String> {
    let out = tokio::time::timeout(
        timeout,
        adb_command()
            .args(["-s", serial, "shell", cmd])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("adb shell timed out"))??;
    if !out.status.success() {
        anyhow::bail!(
            "adb shell '{cmd}' failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `adb -s <serial> shell <cmd>` with the default short timeout.
async fn adb_shell(serial: &str, cmd: &str) -> anyhow::Result<String> {
    adb_shell_with(serial, cmd, Duration::from_secs(5)).await
}

// ── Package installs ─────────────────────────────────────────────────────────

/// True when a path names a file on the phone rather than on this computer.
///
/// An absolute path that does not exist here cannot be a local file, and the
/// phone's storage is the only other place adbshare can reach, so it is
/// treated as a device path. A relative path is always local.
fn is_device_local_path(path: &str) -> bool {
    if path.starts_with("/sdcard/")
        || path.starts_with("/storage/")
        || path.starts_with("/data/")
        || path.starts_with("/mnt/")
    {
        return true;
    }
    path.starts_with('/') && !std::path::Path::new(path).exists()
}

/// True for a string that could only be a package name, which is what keeps a
/// shell injection out of every command that one is interpolated into.
fn is_package_name(package: &str) -> bool {
    if package.is_empty() || package.len() > 224 {
        return false;
    }
    // The real rule: dot-separated segments, each starting with a letter and
    // continuing with letters, digits or underscores. Anything else is not a
    // package name, and refusing it is what keeps a shell injection out of
    // every command one is interpolated into.
    package.split('.').all(|segment| {
        let mut chars = segment.chars();
        match chars.next() {
            Some(first) if first.is_ascii_alphabetic() => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Drop the oldest finished installs so the map stays within
/// [`INSTALL_HISTORY_CAP`]. Callers hold the state lock.
fn prune_installs(installs: &mut HashMap<u64, InstallStatus>) {
    let mut done: Vec<u64> = installs
        .iter()
        .filter(|(_, s)| s.finished)
        .map(|(id, _)| *id)
        .collect();
    if done.len() <= INSTALL_HISTORY_CAP {
        return;
    }
    done.sort_unstable();
    let surplus = done.len() - INSTALL_HISTORY_CAP;
    for id in done.into_iter().take(surplus) {
        installs.remove(&id);
    }
}

/// The one JSON shape `package_info` serves, read back from `dumpsys`.
#[derive(Debug, Clone, Serialize)]
struct PackageInfo {
    package: String,
    installed: bool,
    version_name: Option<String>,
    version_code: Option<u64>,
}

/// Ask the device what it has installed for this package.
///
/// `dumpsys package <pkg>` prints a versionName = line for anything installed
/// and nothing of the kind when it is not, so the shape of the answer carries
/// the answer — no separate existence probe is needed.
async fn read_package_info(device: &str, package: &str) -> anyhow::Result<String> {
    let dump = adb_shell_with(
        device,
        &format!("dumpsys package '{package}'"),
        Duration::from_secs(10),
    )
    .await?;
    // The first occurrences are the installed package's own; later ones belong
    // to shared-user or split entries.
    let mut version_name = None;
    let mut version_code = None;
    for line in dump.lines() {
        let line = line.trim();
        if version_name.is_none()
            && let Some(rest) = line
                .strip_prefix("versionName=")
                .and_then(|v| v.split_whitespace().next())
                .filter(|v| !v.is_empty())
        {
            version_name = Some(rest.to_string());
        }
        if version_code.is_none()
            && let Some(rest) = line
                .strip_prefix("versionCode=")
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.trim().parse().ok())
        {
            version_code = Some(rest);
        }
    }
    let installed = version_name.is_some() || version_code.is_some();
    serde_json::to_string(&PackageInfo {
        package: package.to_string(),
        installed,
        version_name,
        version_code,
    })
    .map_err(|e| anyhow::anyhow!("serialize package info: {e}"))
}

/// What a `pm install` failure means, in code and in person.
///
/// The raw output is never thrown away; the ledger keeps it next to the
/// translation, so the recovery dialog can quote it.
fn classify_install_failure(trimmed: &str) -> (Option<String>, String) {
    if trimmed.contains("INSTALL_FAILED_VERSION_DOWNGRADE") {
        return (
            Some("downgrade".into()),
            "A newer version of this application is already installed on the device.".into(),
        );
    }
    if trimmed.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") {
        return (
            Some("signature_mismatch".into()),
            "Signatures do not match the installed version. The old app has to be uninstalled
             before installing this one; its data will be lost."
                .into(),
        );
    }
    if trimmed.contains("INSTALL_FAILED_INSUFFICIENT_STORAGE") {
        return (
            Some("insufficient_storage".into()),
            "The device does not have enough storage space for this package.".into(),
        );
    }
    if trimmed.contains("INSTALL_FAILED_CONFLICTING_PROVIDER") {
        return (
            Some("conflicting_provider".into()),
            "Another app already registers the same content provider.".into(),
        );
    }
    if trimmed.contains("INSTALL_FAILED_NO_MATCHING_ABIS") {
        return (
            Some("no_matching_abis".into()),
            "This package does not support the device's CPU architecture.".into(),
        );
    }
    if trimmed.contains("INSTALL_PARSE_FAILED") {
        return (
            Some("parse_failed".into()),
            "The file appears damaged or is not a valid APK.".into(),
        );
    }
    let offline = [
        "device not found",
        "no devices",
        "device offline",
        "closed",
        "more than one",
    ]
    .iter()
    .any(|needle| trimmed.to_ascii_lowercase().contains(needle));
    if offline {
        return (
            Some("device_offline".into()),
            "The phone disconnected (or is not authorised for debugging).".into(),
        );
    }
    (Some("unknown".into()), trimmed.to_string())
}

/// Update one install's mutable bookkeeping. The status lock is held only for
/// the field writes, so pipelined adb calls never run inside it.
fn update_install(state: &Arc<Mutex<State>>, id: u64, mutate: impl FnOnce(&mut InstallStatus)) {
    let mut s = state.lock();
    if let Some(status) = s.installs.get_mut(&id) {
        mutate(status);
    }
}

/// Finish an install with a failure and release the pipeline slot.
fn install_failed(
    state: &Arc<Mutex<State>>,
    id: u64,
    code: Option<String>,
    message: String,
    raw: Option<String>,
) {
    add_install_signed(state, Some(id), code, Some(message), raw, false);
}

/// Finish an install with success and release the pipeline slot.
fn install_succeeded(state: &Arc<Mutex<State>>, id: u64, raw: String) {
    add_install_signed(state, Some(id), None, None, Some(raw), true);
}

/// Mark terminal state once, in one place, always clearing `install_running`.
fn add_install_signed(
    state: &Arc<Mutex<State>>,
    id: Option<u64>,
    code: Option<String>,
    message: Option<String>,
    raw: Option<String>,
    success: bool,
) {
    let mut s = state.lock();
    s.install_running = false;
    if let Some(id) = id
        && let Some(status) = s.installs.get_mut(&id)
    {
        status.finished = true;
        status.state = if success { "Done" } else { "Failed" }.into();
        status.stage = 5;
        status.percent = if success { 100 } else { status.percent };
        status.error_code = code;
        status.message = message;
        status.raw = raw;
    }
    prune_installs(&mut s.installs);
}

/// The full sideload install: stage the file (pushing it when it is
/// host-local, with live byte progress), then `pm install -r`, then clean up.
/// Every exit releases the single-install slot exactly once, through
/// [`install_failed`] or [`install_succeeded`].
async fn run_install_pipeline(state: Arc<Mutex<State>>, id: u64, device: String, path: String) {
    let outcome = perform_install(&state, id, &device, &path).await;
    if let Err(err) = &outcome {
        // The pipeline releases the single-install slot on every other exit.
        // An error return is the one path that would leave it held, and a
        // held slot means no install ever runs again until the daemon
        // restarts.
        install_failed(
            &state,
            id,
            Some("unknown".into()),
            format!("the install could not be completed: {err}"),
            Some(err.to_string()),
        );
    }
    debug!(id, ?outcome, "install pipeline finished");
}

/// The body of [`run_install_pipeline`].
async fn perform_install(
    state: &Arc<Mutex<State>>,
    id: u64,
    device: &str,
    path: &str,
) -> anyhow::Result<()> {
    let from_device = {
        let watcher = state.lock();
        watcher
            .installs
            .get(&id)
            .map(|status| status.from_device)
            .unwrap_or(false)
    };

    // ── Stage the file ──────────────────────────────────────────────────
    update_install(state, id, |status| {
        status.state = "Copying".into();
        status.stage = 2;
    });

    if from_device {
        // A device-local APK is copied on the device itself: no transfer to
        // watch, so the card goes straight through the installing stage.
        let escaped = path.replace('\'', "'\\''");
        if let Err(e) = adb_shell_with(
            device,
            &format!("cp '{escaped}' {INSTALL_STAGE}"),
            INSTALL_CP_TIMEOUT,
        )
        .await
        {
            let raw = format!("copying {path} into the staging area: {e}");
            install_failed(state, id, Some("unknown".into()), raw.clone(), Some(raw));
            return Ok(());
        }
    } else {
        // Poll the staged file's size on a second task while `adb push` runs;
        // that is the whole trick to live transfer progress. The poll task is
        // aborted as soon as the push ends, whichever way it went.
        let poll = {
            let state = state.clone();
            let device = device.to_string();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(INSTALL_POLL_INTERVAL).await;
                    let Ok(out) = adb_shell(&device, &format!("stat -c %s {INSTALL_STAGE}")).await
                    else {
                        continue;
                    };
                    let done = out.trim().parse::<u64>().unwrap_or(0);
                    update_install(&state, id, |status| {
                        status.bytes_done = done;
                        if let Some(pct) = (done * 100)
                            .checked_div(status.bytes_total)
                            .map(|p| p.min(100) as u8)
                        {
                            status.percent = pct;
                        }
                    });
                }
            })
        };
        let push = adb_run_output(
            &["-s", device, "push", path, INSTALL_STAGE],
            INSTALL_PUSH_TIMEOUT,
        )
        .await;
        poll.abort();
        let Ok(out) = push else {
            let e = "push never returned".to_string();
            install_failed(state, id, Some("device_offline".into()), e.clone(), Some(e));
            return Ok(());
        };
        if !out.status.success() {
            let raw = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let trimmed = raw.trim();
            let (code, mut message) = classify_install_failure(trimmed);
            if message != trimmed {
                message = format!("{message}\n\n{trimmed}");
            }
            install_failed(state, id, code, message, Some(trimmed.to_string()));
            return Ok(());
        }
        update_install(state, id, |status| {
            status.bytes_done = status.bytes_total;
            status.percent = 100;
        });
    }

    // ── Verify + commit ─────────────────────────────────────────────────
    update_install(state, id, |status| {
        status.state = "Installing".into();
        status.stage = 4;
    });
    let out = tokio::time::timeout(
        INSTALL_PM_TIMEOUT,
        adb_command()
            .args([
                "-s",
                device,
                "shell",
                &format!("pm install -r {INSTALL_STAGE}"),
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    // Clean up the staged copy either way: a 20 MB leftover in
    // /data/local/tmp is a small thing, but it is ours.
    let _ = adb_shell(device, &format!("rm -f {INSTALL_STAGE}")).await;
    match out {
        Err(_) => {
            install_failed(
                state,
                id,
                Some("timeout".into()),
                "pm install did not answer in time.".into(),
                None,
            );
        }
        Ok(Err(e)) => {
            let raw = format!("pm install: {e}");
            install_failed(
                state,
                id,
                Some("device_offline".into()),
                raw.clone(),
                Some(raw),
            );
        }
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let trimmed = text.trim();
            let is_failure = !out.status.success()
                || trimmed.contains("Failure [")
                || trimmed.starts_with("Failure")
                || trimmed.contains("INSTALL_FAILED");
            if is_failure {
                let (code, message) = classify_install_failure(trimmed);
                install_failed(
                    state,
                    id,
                    code,
                    format!("{message}\n\n{trimmed}"),
                    Some(trimmed.to_string()),
                );
            } else {
                install_succeeded(state, id, trimmed.to_string());
            }
        }
    }
    Ok(())
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
    /// "usb" or "wifi", inferred from the serial format.
    transport: &'static str,
    battery_pct: Option<u8>,
    storage_used: Option<u64>,
    storage_total: Option<u64>,
    /// Primary CPU architecture, e.g. `arm64-v8a`, for the install card's
    /// ABI warning. The GUI matches it against the APK's native libs.
    abi: Option<String>,
}

async fn device_info_json(serial: &str) -> anyhow::Result<String> {
    let transport = if is_wireless_serial(serial) {
        "wifi"
    } else {
        "usb"
    };

    let (props_raw, battery_raw, df_raw) = tokio::join!(
        adb_shell(serial, "getprop"),
        adb_shell(serial, "dumpsys battery"),
        adb_shell(serial, "df -k /sdcard"),
    );

    let abi = match &props_raw {
        Ok(raw) => parse_getprop(raw)
            .get("ro.product.cpu.abi")
            .filter(|s| !s.is_empty())
            .cloned(),
        Err(_) => None,
    };

    let model = match props_raw {
        Ok(raw) => {
            let props = parse_getprop(&raw);
            props
                .get("ro.product.marketname")
                .filter(|s| !s.is_empty() && s.as_str() != "UNRECOGNIZED")
                .or_else(|| props.get("ro.product.model"))
                .filter(|s| !s.is_empty())
                .cloned()
        }
        Err(e) => {
            warn!(serial, error = %e, "getprop failed");
            None
        }
    };

    let battery_pct = battery_raw.ok().and_then(|raw| {
        for line in raw.lines() {
            let line = line.trim();
            if let Some(v) = line
                .strip_prefix("level:")
                .and_then(|v| v.trim().parse::<u8>().ok())
            {
                return Some(v.min(100));
            }
        }
        None
    });

    // toybox `df -k` last line: Filesystem 1K-blocks Used Available Use% Mounted
    let (storage_used, storage_total) = match df_raw {
        Ok(raw) => match raw.lines().rfind(|l| !l.trim().is_empty()) {
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
        transport,
        battery_pct,
        storage_used,
        storage_total,
        abi,
    })
    .map_err(|e| anyhow::anyhow!("serialize device info: {e}"))
}

fn client_for(
    state: &Arc<Mutex<State>>,
    device: &str,
) -> Result<Arc<ProxyClient>, zbus::fdo::Error> {
    state
        .lock()
        .devices
        .get(&DeviceId(device.to_string()))
        .map(|slot| slot.client.clone())
        .ok_or_else(|| zbus::fdo::Error::ServiceUnknown("device not connected".into()))
}

/// Maximum recursion depth for `delete_recursive`; beyond this we assume a
/// cycle or a pathological tree and refuse rather than recurse forever.
const DELETE_MAX_DEPTH: u32 = 64;

/// Depth cap shared by the three tree operations (`enqueue_tree_push`,
/// `enqueue_tree_pull`, `copy_tree`). Lower than `DELETE_MAX_DEPTH` because a
/// delete has to reach every node, while these can stop and report what they
/// skipped. Matches the limit the README documents.
const TREE_MAX_DEPTH: u32 = 32;

/// Files a single tree operation may touch, across all of its directories.
/// A tree can be deep but also *wide*, so this is counted per entry rather
/// than per directory — checked at the top of the loop it never fired for a
/// single wide directory, and one bus call could drive unbounded round trips.
const TREE_MAX_FILES: usize = 10_000;

/// Join one directory listing entry to its parent. Device paths are always
/// absolute and never end in `/` except for the root, so the trim keeps the
/// root from becoming `//name`.
fn child_path(parent: &str, name: &str) -> String {
    format!("{}/{}", parent.trim_end_matches('/'), name)
}

/// Validate one path handed to a tree operation by the bus. All three take
/// caller-supplied absolute paths and none of them get to inspect what the
/// device does with the result, so the checks live here rather than being
/// repeated (and forgotten) per handler: absolute, no NUL, and within the
/// `PATH_MAX` the device's own syscalls would enforce anyway.
fn check_tree_path(path: &str) -> zbus::fdo::Result<()> {
    const MAX_PATH: usize = 4096;
    if !path.starts_with('/') {
        return Err(zbus::fdo::Error::InvalidArgs(
            "tree paths must be absolute".into(),
        ));
    }
    if path.contains('\0') || path.len() > MAX_PATH {
        return Err(zbus::fdo::Error::InvalidArgs(format!(
            "tree paths must be non-NUL and at most {MAX_PATH} bytes"
        )));
    }
    Ok(())
}

/// `Some(reason)` once `count` has reached `TREE_MAX_FILES`. The handlers stop
/// and serialise what they got, so the partial result reaches the GUI rather
/// than the whole batch being discarded over one oversized folder.
fn tree_file_cap_error(count: usize) -> Option<String> {
    (count >= TREE_MAX_FILES).then(|| format!("too many files (limit {TREE_MAX_FILES})"))
}

/// `Some(reason)` once a walk has descended past `TREE_MAX_DEPTH`.
fn tree_depth_error(path: &str, depth: u32) -> Option<String> {
    (depth >= TREE_MAX_DEPTH).then(|| format!("{path}: directory nesting too deep"))
}

/// Serialise a tree result. Every handler returns this, including on the
/// early exit that a cap triggers.
fn tree_result(out: &TreeEnqueueResult) -> zbus::fdo::Result<String> {
    serde_json::to_string(out).map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
}

/// Recursively delete `path` on the device via proxy ops (there is no
/// server-side `rm -r` in the proxy protocol). `depth` is capped at
/// `DELETE_MAX_DEPTH`.
async fn delete_recursive(client: &ProxyClient, path: &str, depth: u32) -> anyhow::Result<()> {
    if depth > DELETE_MAX_DEPTH {
        anyhow::bail!("delete: recursion deeper than {DELETE_MAX_DEPTH} levels at '{path}'");
    }
    let st = client.lstat(path).await?;
    if st.mode.is_symlink() || !st.mode.is_dir() {
        return client
            .unlink(path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }
    for entry in client.listdir(path).await? {
        let child = child_path(path, &entry.name);
        Box::pin(delete_recursive(client, &child, depth + 1)).await?;
    }
    client.rmdir(path).await.map_err(|e| anyhow::anyhow!("{e}"))
}

/// Cached `device_info` payloads, per serial.
///
/// The GUI asks for device metadata every three seconds, and each answer costs
/// three `adb shell` subprocesses on the phone — `getprop` dumps every system
/// property, `dumpsys battery` is a binder round trip, and `df` stats the
/// volume. That is a subprocess per second per device, which shows up as
/// background CPU and battery drain on a phone that is otherwise idle, and it
/// buys nothing: the model never changes, and the battery percentage moves at
/// most once in a while.
///
/// Entries are refreshed on expiry and the last good value is served if a
/// refresh fails, so a phone that has gone quiet does not make the sidebar
/// flicker.
struct TimedCache {
    ttl: Duration,
    /// `parking_lot` rather than `std::sync::Mutex` on purpose: a panic while the
    /// guard is held poisons a std mutex, and these methods used to swallow that
    /// with `.ok()`, which left the cache permanently dead and silently put back
    /// the three `adb shell` subprocesses per poll this cache exists to avoid.
    /// A parking_lot lock is never poisoned, so the worst case is one lost
    /// update.
    entries: Mutex<std::collections::HashMap<String, (Instant, String)>>,
}

impl TimedCache {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The cached payload for `serial`, if it is still fresh.
    fn fresh(&self, serial: &str) -> Option<String> {
        let entries = self.entries.lock();
        let (when, json) = entries.get(serial)?;
        (when.elapsed() < self.ttl).then(|| json.clone())
    }

    /// The cached payload for `serial` however old it is.
    fn stale(&self, serial: &str) -> Option<String> {
        self.entries
            .lock()
            .get(serial)
            .map(|(_, json)| json.clone())
    }

    fn store(&self, serial: &str, json: String) {
        self.entries
            .lock()
            .insert(serial.to_string(), (Instant::now(), json));
    }

    /// Forget a device that has gone away, so the cache cannot grow without
    /// bound over a long session with phones coming and going.
    fn forget_missing(&self, present: &[String]) {
        self.entries
            .lock()
            .retain(|serial, _| present.iter().any(|known| known == serial));
    }

    /// Drop one entry, so the next read goes back to the device.
    fn forget(&self, key: &str) {
        self.entries.lock().remove(key);
    }

    /// Forget the readings of devices that have gone away, so the cache cannot
    /// grow without bound over a long session with phones coming and going.
    ///
    /// A key is a device serial, or a device serial and a package joined by a
    /// null byte; in both cases the device is the part before the separator.
    fn forget_missing_devices(&self, present: &[String]) {
        self.entries.lock().retain(|key, _| {
            let device = key.split('\u{0}').next().unwrap_or(key);
            present.iter().any(|known| known == device)
        });
    }
}

struct ManagerInterface {
    state: Arc<Mutex<State>>,
    queue: Arc<JobQueue>,
    device_info: Arc<TimedCache>,
    /// Cached `package_info` readings, per device+package, for the same reason
    /// as `device_info`: a pre-install card asks once per APK, and `dumpsys`
    /// costs a binder round trip per call.
    package_info: Arc<TimedCache>,
    /// Cached package inventories, per device and user. The GUI refreshes the
    /// list often, and each reading costs four `pm` calls on the phone.
    packages: Arc<TimedCache>,
}

#[interface(name = "org.adbshare.Manager")]
impl ManagerInterface {
    /// List all devices currently set up (mounted + proxy connected).
    async fn list_devices(&self) -> zbus::fdo::Result<Vec<String>> {
        let devices: Vec<String> = {
            let s = self.state.lock();
            s.devices.keys().map(|d| d.to_string()).collect()
        };
        self.device_info.forget_missing(&devices);
        self.package_info.forget_missing_devices(&devices);
        Ok(devices)
    }

    /// Live device metadata (model, transport, battery, storage) gathered
    /// over `adb shell`. Returns a JSON `DeviceInfoDto`, cached briefly because
    /// the GUI polls it far more often than the answers change.
    async fn device_info(&self, serial: String) -> zbus::fdo::Result<String> {
        if let Some(cached) = self.device_info.fresh(&serial) {
            return Ok(cached);
        }
        match device_info_json(&serial).await {
            Ok(json) => {
                self.device_info.store(&serial, json.clone());
                Ok(json)
            }
            Err(e) => {
                // A phone that has gone quiet should not make the sidebar empty
                // out; the previous reading is better than nothing.
                match self.device_info.stale(&serial) {
                    Some(cached) => {
                        tracing::debug!(%serial, error = %e, "device_info refresh failed; serving the cached reading");
                        Ok(cached)
                    }
                    None => Err(zbus::fdo::Error::Failed(format!("device_info: {e}"))),
                }
            }
        }
    }

    /// FUSE mountpoint for a device, if mounted.
    async fn mountpoint_for(&self, device: &str) -> zbus::fdo::Result<String> {
        let s = self.state.lock();
        s.devices
            .get(&DeviceId(device.to_string()))
            .and_then(|slot| slot.mountpoint.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| zbus::fdo::Error::ServiceUnknown("device not mounted".into()))
    }

    /// List a directory on a device. Returns a JSON array of entries so we
    /// don't have to plumb zvariant types through.
    async fn list_dir(&self, device: &str, path: &str) -> zbus::fdo::Result<String> {
        tracing::info!(%device, %path, "list_dir called");
        let client = {
            let s = self.state.lock();
            s.devices
                .get(&DeviceId(device.to_string()))
                .map(|slot| slot.client.clone())
                .ok_or_else(|| zbus::fdo::Error::ServiceUnknown("device not connected".into()))?
        };
        let entries = client
            .listdir(path)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("listdir: {e}")))?;
        tracing::info!(%device, %path, n = entries.len(), "list_dir done");
        let mut dtos: Vec<DirEntryDto> = Vec::with_capacity(entries.len());
        for e in entries {
            let mut dto = DirEntryDto::from(e);
            if dto.is_symlink && !dto.is_dir {
                let full = child_path(path, &dto.name);
                if let Ok(st) = client.stat(&full).await
                    && st.mode.is_dir()
                {
                    dto.is_dir = true;
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

    async fn enqueue_push_with_options(
        &self,
        device: &str,
        local_path: &str,
        device_path: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::fdo::Result<u64> {
        let options = parse_job_options(overwrite, verify)?;
        let job = Job::with_device(
            0,
            Direction::Push,
            PathBuf::from(local_path),
            PathBuf::from(device_path),
            options,
            Some(device.to_string()),
        );
        Ok(self.queue.submit(job))
    }

    async fn enqueue_pull_with_options(
        &self,
        device: &str,
        device_path: &str,
        local_path: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::fdo::Result<u64> {
        let options = parse_job_options(overwrite, verify)?;
        let job = Job::with_device(
            0,
            Direction::Pull,
            PathBuf::from(device_path),
            PathBuf::from(local_path),
            options,
            Some(device.to_string()),
        );
        Ok(self.queue.submit(job))
    }

    async fn enqueue_tree_push(
        &self,
        device: &str,
        local_dir: &str,
        device_dir: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::fdo::Result<String> {
        let options = parse_job_options(overwrite, verify)?;
        check_tree_path(local_dir)?;
        check_tree_path(device_dir)?;
        let local_base = PathBuf::from(local_dir);
        let device_base = PathBuf::from(device_dir);
        let client = client_for(&self.state, device)?;
        let mut out = TreeEnqueueResult::default();
        let mut stack = vec![(local_base.clone(), device_base.clone(), 0u32)];
        while let Some((local, remote, depth)) = stack.pop() {
            if let Some(e) = tree_depth_error(&local.display().to_string(), depth) {
                out.errors.push(e);
                continue;
            }
            let read = std::fs::read_dir(&local)
                .map_err(|e| zbus::fdo::Error::Failed(format!("read {}: {e}", local.display())))?;
            for entry in read {
                let entry = entry.map_err(|e| {
                    zbus::fdo::Error::Failed(format!("read {}: {e}", local.display()))
                })?;
                if let Some(e) = tree_file_cap_error(out.enqueued.len()) {
                    out.errors.push(e);
                    return tree_result(&out);
                }
                let file_type = entry.file_type().map_err(|e| {
                    zbus::fdo::Error::Failed(format!("stat {}: {e}", entry.path().display()))
                })?;
                if file_type.is_symlink() {
                    continue;
                }
                let remote_child = remote.join(entry.file_name());
                let remote_str = remote_child.to_string_lossy().into_owned();
                if file_type.is_dir() {
                    let _ = client.mkdir(&remote_str, 0o755).await;
                    stack.push((entry.path(), remote_child, depth + 1));
                } else if file_type.is_file() {
                    let job = Job::with_device(
                        0,
                        Direction::Push,
                        entry.path(),
                        PathBuf::from(remote_str),
                        options.clone(),
                        Some(device.to_string()),
                    );
                    out.enqueued.push(self.queue.submit(job));
                }
            }
        }
        tree_result(&out)
    }

    async fn enqueue_tree_pull(
        &self,
        device: &str,
        device_dir: &str,
        local_dir: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::fdo::Result<String> {
        let options = parse_job_options(overwrite, verify)?;
        check_tree_path(device_dir)?;
        check_tree_path(local_dir)?;
        let local_base = PathBuf::from(local_dir);
        std::fs::create_dir_all(&local_base).map_err(|e| {
            zbus::fdo::Error::Failed(format!("mkdir {}: {e}", local_base.display()))
        })?;
        let client = client_for(&self.state, device)?;
        let mut out = TreeEnqueueResult::default();
        let mut stack = vec![(device_dir.to_string(), local_base, 0u32)];
        while let Some((remote, local, depth)) = stack.pop() {
            if let Some(e) = tree_depth_error(&remote, depth) {
                out.errors.push(e);
                continue;
            }
            let entries = client
                .listdir(&remote)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("list {remote}: {e}")))?;
            for entry in entries {
                if let Some(e) = tree_file_cap_error(out.enqueued.len()) {
                    out.errors.push(e);
                    return tree_result(&out);
                }
                if entry.stat.mode.is_symlink() {
                    continue;
                }
                let remote_child = child_path(&remote, &entry.name);
                let local_child = local.join(&entry.name);
                if entry.stat.mode.is_dir() {
                    std::fs::create_dir_all(&local_child).map_err(|e| {
                        zbus::fdo::Error::Failed(format!("mkdir {}: {e}", local_child.display()))
                    })?;
                    stack.push((remote_child, local_child, depth + 1));
                } else {
                    let job = Job::with_device(
                        0,
                        Direction::Pull,
                        PathBuf::from(remote_child),
                        local_child,
                        options.clone(),
                        Some(device.to_string()),
                    );
                    out.enqueued.push(self.queue.submit(job));
                }
            }
        }
        tree_result(&out)
    }

    async fn copy_tree(
        &self,
        device: &str,
        src_dir: &str,
        dst_dir: &str,
    ) -> zbus::fdo::Result<String> {
        check_tree_path(src_dir)?;
        check_tree_path(dst_dir)?;
        if src_dir == dst_dir {
            return Err(zbus::fdo::Error::InvalidArgs(
                "source and destination are the same".into(),
            ));
        }
        let client = client_for(&self.state, device)?;
        let mut out = TreeEnqueueResult::default();
        let mut stack = vec![(src_dir.to_string(), dst_dir.to_string(), 0u32)];
        while let Some((src, dst, depth)) = stack.pop() {
            if let Some(e) = tree_depth_error(&src, depth) {
                out.errors.push(e);
                continue;
            }
            let _ = client.mkdir(&dst, 0o755).await;
            let entries = client
                .listdir(&src)
                .await
                .map_err(|e| zbus::fdo::Error::Failed(format!("list {src}: {e}")))?;
            for entry in entries {
                // Copied files count, not just enqueued ids: this handler never
                // touches the queue.
                if let Some(e) = tree_file_cap_error(out.copied + out.errors.len()) {
                    out.errors.push(e);
                    return tree_result(&out);
                }
                if entry.stat.mode.is_symlink() {
                    continue;
                }
                let src_child = child_path(&src, &entry.name);
                let dst_child = child_path(&dst, &entry.name);
                if entry.stat.mode.is_dir() {
                    stack.push((src_child, dst_child, depth + 1));
                } else {
                    match client.copy_file(&src_child, &dst_child).await {
                        Ok(()) => out.copied += 1,
                        Err(e) => out.errors.push(format!("copy {src_child}: {e}")),
                    }
                }
            }
        }
        tree_result(&out)
    }

    async fn diagnostics(&self) -> zbus::fdo::Result<String> {
        let (devices, adb_server, mount_base, proxy_conns, no_fuse, auth_key_path) = {
            let state = self.state.lock();
            let devices = state
                .devices
                .iter()
                .map(|(id, slot)| DeviceDiag {
                    serial: id.0.clone(),
                    mounted: slot.mountpoint.is_some(),
                })
                .collect();
            (
                devices,
                state.adb_server.clone(),
                state.mount_base.clone(),
                state.proxy_conns,
                state.no_fuse,
                state
                    .auth_key
                    .as_ref()
                    .map(|k| k.path().to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        };
        let adb_output = tokio::time::timeout(
            Duration::from_secs(5),
            adb_command().arg("version").kill_on_drop(true).output(),
        )
        .await;
        let (adb_ok, adb_version) = match adb_output {
            Ok(Ok(out)) if out.status.success() => {
                let first_line = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                (true, first_line)
            }
            _ => (false, String::new()),
        };
        let helper_env = std::env::var_os("ADBSHARE_PROXY_BIN").map(PathBuf::from);
        let report = DiagnosticReport {
            adb_ok,
            adb_version,
            adb_server,
            mount_base: mount_base.to_string_lossy().into_owned(),
            proxy_conns,
            no_fuse,
            helper_env_present: helper_env.is_some(),
            helper_env_exists: helper_env.as_ref().is_some_and(|p| p.is_file()),
            auth_key_path,
            devices,
        };
        serde_json::to_string(&report)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    /// Snapshot of every job currently tracked by the queue (JSON).
    async fn list_jobs(&self) -> zbus::fdo::Result<String> {
        let dtos: Vec<JobDto> = self
            .queue
            .jobs_snapshot()
            .into_iter()
            .map(JobDto::from)
            .collect();
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

    /// Requeue every failed job as pending so the worker loop retries it.
    /// Returns the number of jobs requeued.
    async fn retry_failed(&self) -> zbus::fdo::Result<u64> {
        Ok(self.queue.retry_failed())
    }

    // --- File operations on a device (via the on-device proxy) ---

    async fn mkdir(&self, device: &str, path: &str) -> zbus::fdo::Result<()> {
        let client = client_for(&self.state, device)?;
        client
            .mkdir(path, 0o755)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("mkdir: {e}")))
    }

    async fn rename(&self, device: &str, src: &str, dst: &str) -> zbus::fdo::Result<()> {
        let client = client_for(&self.state, device)?;
        client
            .rename(src, dst)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("rename: {e}")))
    }

    async fn copy_file(&self, device: &str, src: &str, dst: &str) -> zbus::fdo::Result<()> {
        for path in [src, dst] {
            if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "copy paths must be absolute, non-NUL, and at most 4096 bytes".into(),
                ));
            }
        }
        let client = client_for(&self.state, device)?;
        client
            .copy_file(src, dst)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("copy_file: {e}")))
    }

    /// Delete a file or directory tree on the device.
    async fn delete(&self, device: &str, path: &str) -> zbus::fdo::Result<()> {
        // Refuse to delete the device root (directly or via `..` / `//`
        // trickery) — D-Bus callers must delete concrete subtrees.
        let normalized: Vec<&str> = path
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        if normalized.is_empty() || normalized.contains(&"..") {
            return Err(zbus::fdo::Error::InvalidArgs(
                "refusing to delete the device root".into(),
            ));
        }
        let client = client_for(&self.state, device)?;
        delete_recursive(&client, path, 0)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("delete: {e}")))
    }

    /// `adb connect <address>` — returns the adb CLI output so the GUI can
    /// surface it. The address must look like host:port.
    async fn connect_wireless(&self, address: &str) -> zbus::fdo::Result<String> {
        validate_host_port(address)?;
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            adb_command()
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

    /// `adb pair <address> <code>` with a 60s timeout. The address must look
    /// like host:port and the code must be six digits.
    async fn pair_wireless(&self, address: &str, code: &str) -> zbus::fdo::Result<String> {
        validate_host_port(address)?;
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "pairing code must be six digits".into(),
            ));
        }
        let out = tokio::time::timeout(
            Duration::from_secs(60),
            adb_command()
                .args(["pair", address, code])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| zbus::fdo::Error::Failed(format!("adb pair {address} timed out")))?
        .map_err(|e| zbus::fdo::Error::Failed(format!("adb pair: {e}")))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        if !out.status.success() {
            let message = if text.trim().is_empty() {
                format!("adb pair failed with status {status}", status = out.status)
            } else {
                text
            };
            return Err(zbus::fdo::Error::Failed(message));
        }
        Ok(text)
    }

    /// Begin installing an APK from a local (host) or device path.
    ///
    /// Returns the install id the GUI can watch for with `install_status_all`.
    /// The pipeline runs in the background: push the file to
    /// `/data/local/tmp/adbshare_install_stage.apk` (with byte progress read
    /// back by polling `stat` on the device), then `pm install -r`, then
    /// remove the staged copy. That decomposition is what makes progress
    /// possible at all: `adb install` does its own push internally and gives
    /// nothing back until it has fully finished or failed.
    async fn start_install(&self, device: &str, path: &str) -> zbus::fdo::Result<u64> {
        if device.is_empty() || path.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "device and path are required".into(),
            ));
        }
        client_for(&self.state, device)?;

        let from_device = is_device_local_path(path);
        let host_exists = std::path::Path::new(path).exists();
        if !from_device && !host_exists {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "{path} does not exist on this computer"
            )));
        }

        let mut state = self.state.lock();
        if state.install_running {
            return Err(zbus::fdo::Error::Failed(
                "another installation is already running; wait for it to finish".into(),
            ));
        }
        let id = state.next_install_id;
        state.next_install_id += 1;
        state.install_running = true;
        let label = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        state.installs.insert(
            id,
            InstallStatus {
                id,
                device: device.to_string(),
                label,
                source: path.to_string(),
                from_device,
                stage: 0,
                percent: 0,
                bytes_done: 0,
                bytes_total: if from_device || !host_exists {
                    0
                } else {
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                },
                state: "Pending".into(),
                error_code: None,
                message: None,
                raw: None,
                finished: false,
            },
        );
        prune_installs(&mut state.installs);
        drop(state);

        let state = self.state.clone();
        let device = device.to_string();
        let path = path.to_string();
        tokio::spawn(async move {
            run_install_pipeline(state, id, device, path).await;
        });
        Ok(id)
    }

    /// Every tracked install status, latest first. The GUI polls this on its
    /// regular job tick while an install is on screen.
    async fn install_status_all(&self) -> zbus::fdo::Result<String> {
        let mut statuses: Vec<InstallStatus> =
            self.state.lock().installs.values().cloned().collect();
        statuses.sort_by_key(|s| std::cmp::Reverse(s.id));
        serde_json::to_string(&statuses)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    /// What is installed for this package, read back from `dumpsys command
    /// package`. Powers the pre-install card's upgrade/downgrade warning.
    async fn package_info(&self, device: &str, package: &str) -> zbus::fdo::Result<String> {
        if device.is_empty() || package.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "device and package are required".into(),
            ));
        }
        if !is_package_name(package) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "{package} does not look like a package name"
            )));
        }
        let key = format!("{device}\u{0}{package}");
        if let Some(cached) = self.package_info.fresh(&key) {
            return Ok(cached);
        }
        match read_package_info(device, package).await {
            Ok(json) => {
                self.package_info.store(&key, json.clone());
                Ok(json)
            }
            Err(e) => {
                if let Some(cached) = self.package_info.stale(&key) {
                    tracing::debug!(%device, error = %e, "package_info refresh failed; serving the cached reading");
                    return Ok(cached);
                }
                Err(zbus::fdo::Error::Failed(format!("package_info: {e}")))
            }
        }
    }

    /// Every package on the device, with its state.
    ///
    /// The answer is a JSON array of `PackageRow`. It is cached, because the
    /// GUI refreshes the list often and each reading costs four `pm` calls on
    /// the phone.
    async fn list_packages(&self, device: &str, user: u16) -> zbus::fdo::Result<String> {
        if device.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("device is required".into()));
        }
        client_for(&self.state, device)?;
        let key = format!("{device}\u{0}{user}");
        if let Some(cached) = self.packages.fresh(&key) {
            return Ok(cached);
        }
        match packages::inventory(device, user).await {
            Ok(rows) => {
                let json = serde_json::to_string(&rows)
                    .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))?;
                self.packages.store(&key, json.clone());
                Ok(json)
            }
            Err(e) => {
                // A phone that has gone quiet should not empty the list; the
                // last good reading is better than nothing.
                if let Some(cached) = self.packages.stale(&key) {
                    tracing::debug!(%device, error = %e, "list_packages refresh failed; serving the cached reading");
                    return Ok(cached);
                }
                Err(zbus::fdo::Error::Failed(format!("list_packages: {e}")))
            }
        }
    }

    /// Change the state of a batch of packages.
    ///
    /// `actions` is a JSON array of `{ "package": ..., "action": ... }`, where
    /// the action is one of `disable`, `enable`, `uninstall` or `restore`. The
    /// answer is a JSON array of `PackageResult`, one per request, in the
    /// order the requests were given.
    ///
    /// The calls run a few at a time, so a list of two hundred packages does
    /// not take two hundred round trips in a row.
    async fn apply_package_actions(
        &self,
        device: &str,
        user: u16,
        actions: &str,
    ) -> zbus::fdo::Result<String> {
        if device.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("device is required".into()));
        }
        client_for(&self.state, device)?;
        let requests: Vec<packages::PackageRequest> = serde_json::from_str(actions)
            .map_err(|e| zbus::fdo::Error::InvalidArgs(format!("actions: {e}")))?;
        if requests.len() > MAX_PACKAGE_ACTIONS {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "at most {MAX_PACKAGE_ACTIONS} packages can be changed at once"
            )));
        }
        let results = packages::apply_all(device, user, requests).await;
        // The device now reports something different, so the cached reading
        // of it is stale.
        self.packages.forget(&format!("{device}\u{0}{user}"));
        serde_json::to_string(&results)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    /// Remove an installed package from a device. `pm uninstall` works for a
    /// normal user app; a delete is offered by recovery flows only.
    async fn uninstall_package(&self, device: &str, package: &str) -> zbus::fdo::Result<()> {
        if device.is_empty() || package.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "device and package are required".into(),
            ));
        }
        if !is_package_name(package) {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "{package} does not look like a package name"
            )));
        }
        client_for(&self.state, device)?;
        let out = adb_shell_with(
            device,
            &format!("pm uninstall '{package}'"),
            UNINSTALL_TIMEOUT,
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("uninstall: {e}")))?;
        // `pm uninstall` prints `Success` on victory and `Failure [...]` with
        // a reason otherwise; an empty output means the adb call itself
        // returned nothing, which is nearly always a disconnect.
        let trimmed = out.trim();
        if trimmed != "Success" {
            return Err(zbus::fdo::Error::Failed(format!(
                "uninstall {package}: {}",
                if trimmed.is_empty() {
                    "device did not answer"
                } else {
                    trimmed
                }
            )));
        }
        Ok(())
    }
}

#[allow(dead_code)]
fn which_(name: &str) -> std::result::Result<PathBuf, std::io::Error> {
    let path = std::env::var_os("PATH").ok_or_else(|| std::io::Error::other("no PATH"))?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "not found",
    ))
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
struct DeviceDiag {
    serial: String,
    mounted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiagnosticReport {
    adb_ok: bool,
    adb_version: String,
    adb_server: String,
    mount_base: String,
    proxy_conns: usize,
    no_fuse: bool,
    helper_env_present: bool,
    helper_env_exists: bool,
    auth_key_path: String,
    devices: Vec<DeviceDiag>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TreeEnqueueResult {
    #[serde(default)]
    enqueued: Vec<u64>,
    /// Files copied synchronously by `copy_tree`. That handler performs the
    /// copies on the device via `Op::CopyFile` and never touches the queue, so
    /// it has no job ids to report — it used to invent `1..n`, which collided
    /// with real queue ids (`JobQueue::next_id` also starts at 1) and so made
    /// `cancel_job`/`pause_job` on one of these act on an unrelated transfer.
    #[serde(default)]
    copied: usize,
    #[serde(default)]
    errors: Vec<String>,
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
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    device: Option<String>,
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
            error: j.error(),
            device: j.device.clone(),
        }
    }
}

fn validate_host_port(address: &str) -> zbus::fdo::Result<()> {
    let valid = address.rsplit_once(':').is_some_and(|(host, port)| {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let Ok(port) = port.parse::<u16>() else {
            return false;
        };
        if port == 0 {
            return false;
        }
        let host = if let Some(host) = host.strip_prefix('[') {
            host.strip_suffix(']').unwrap_or("")
        } else if host.ends_with(']') {
            ""
        } else {
            host
        };
        !host.is_empty()
            && !host.contains(['[', ']'])
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'))
    });
    if valid {
        Ok(())
    } else {
        Err(zbus::fdo::Error::InvalidArgs(
            "address must be host:port".into(),
        ))
    }
}

fn parse_job_options(overwrite: &str, verify: bool) -> zbus::fdo::Result<JobOptions> {
    let overwrite = match overwrite {
        "skip" => transfer_engine::job::OverwriteMode::SkipExisting,
        "replace" => transfer_engine::job::OverwriteMode::Always,
        "keep-both" => transfer_engine::job::OverwriteMode::Rename,
        _ => {
            return Err(zbus::fdo::Error::InvalidArgs(
                "overwrite must be skip, replace, or keep-both".into(),
            ));
        }
    };
    Ok(JobOptions {
        overwrite,
        verify: if verify {
            transfer_engine::job::VerifyMode::On
        } else {
            transfer_engine::job::VerifyMode::Off
        },
        ..JobOptions::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // These four helpers carry the tree operations' only two off-by-one
    // risks, and both have been wrong before: the file cap admitted
    // `TREE_MAX_FILES + 1` and the depth cap one level more than the README
    // documents. They are pure, so they are worth checking directly rather
    // than through a device.

    #[test]
    fn child_path_never_doubles_the_slash() {
        assert_eq!(
            child_path("/sdcard/Download", "a.txt"),
            "/sdcard/Download/a.txt"
        );
        // The root is the case a plain `format!("{}/{}")` got wrong.
        assert_eq!(child_path("/", "a.txt"), "/a.txt");
        // A caller that already joined a trailing slash must not yield `//`.
        assert_eq!(child_path("/sdcard/", "a.txt"), "/sdcard/a.txt");
    }

    #[test]
    fn file_cap_trips_at_the_limit_not_past_it() {
        assert!(tree_file_cap_error(TREE_MAX_FILES - 1).is_none());
        assert!(tree_file_cap_error(TREE_MAX_FILES).is_some());
        assert!(tree_file_cap_error(TREE_MAX_FILES + 1).is_some());
    }

    #[test]
    fn depth_cap_admits_exactly_the_documented_levels() {
        // Depth 0 is the root itself, so `TREE_MAX_DEPTH` levels of nesting
        // means the last directory we may descend into is `TREE_MAX_DEPTH - 1`.
        assert!(tree_depth_error("/a", 0).is_none());
        assert!(tree_depth_error("/a", TREE_MAX_DEPTH - 1).is_none());
        assert!(tree_depth_error("/a", TREE_MAX_DEPTH).is_some());
    }

    #[test]
    fn tree_paths_must_be_absolute_nul_free_and_bounded() {
        assert!(check_tree_path("/sdcard/Download").is_ok());
        assert!(check_tree_path("relative/path").is_err());
        assert!(check_tree_path("/sdcard/\0evil").is_err());
        assert!(check_tree_path(&format!("/{}", "x".repeat(4096))).is_err());
    }

    #[test]
    fn package_names_follow_the_platform_rule() {
        assert!(is_package_name("com.samsung.android.bixby.service"));
        assert!(is_package_name("com.example.app_2"));
        assert!(is_package_name("android"));
        // A segment may not start with a digit or an underscore, and a
        // hyphen is not part of the grammar at all.
        assert!(!is_package_name("com.2fast.app"));
        assert!(!is_package_name("com.example.-app"));
        assert!(!is_package_name("com.example.app;rm -rf /"));
        assert!(!is_package_name("com.example.app$(id)"));
        assert!(!is_package_name(""));
        assert!(!is_package_name("com..app"));
        assert!(!is_package_name(&"a".repeat(225)));
    }

    #[test]
    fn a_missing_local_path_is_not_mistaken_for_a_device_path() {
        // The old rule treated every path that did not exist here as a path
        // on the phone, which made the "does not exist on this computer"
        // error unreachable.
        assert!(!is_device_local_path("does-not-exist.apk"));
        assert!(!is_device_local_path("relative/does-not-exist.apk"));
        assert!(is_device_local_path("/sdcard/Download/app.apk"));
        assert!(is_device_local_path("/storage/emulated/0/app.apk"));
        assert!(is_device_local_path("/data/local/tmp/app.apk"));
        // An absolute path that is not on this computer is on the phone.
        assert!(is_device_local_path("/somewhere/else/app.apk"));
    }
}
