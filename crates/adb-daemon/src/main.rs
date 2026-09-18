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
    /// Whether a full setup (push, forward, proxy start) completed for this
    /// device. Re-add/Changed events use it to decide between a health check
    /// and a full (proxy-killing) re-setup.
    setup_ok: bool,
}

#[derive(Debug, Default)]
struct State {
    /// serial -> slot
    devices: HashMap<DeviceId, DeviceSlot>,
    adb_server: String,
    mount_base: PathBuf,
    proxy_conns: usize,
    no_fuse: bool,
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

/// Parse an `--adb-server` "host:port" spec. Handles bracketed IPv6 hosts
/// like `[::1]:5037`, which a naive `split(':')` would shred. Falls back to
/// the adb default port 5037 when no (valid) port is present.
fn parse_adb_server(spec: &str) -> (String, u16) {
    if let Some(rest) = spec.strip_prefix('[') {
        if let Some((host, after)) = rest.split_once(']') {
            let port = after
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(5037);
            return (host.to_string(), port);
        }
    }
    match spec.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(5037)),
        None => (spec.to_string(), 5037),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHelper {
        dir: PathBuf,
        child: Option<std::process::Child>,
    }

    impl TestHelper {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "adbshare-dbus-copy-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            ));
            std::fs::create_dir(&dir).unwrap();
            Self { dir, child: None }
        }
    }

    impl Drop for TestHelper {
        fn drop(&mut self) {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    #[ignore = "requires dbus-run-session -- env ADBSHARE_TEST_PROXY_BIN=/absolute/path/to/adbshare-proxy cargo test -p adb-daemon --locked copy_file_over_dbus_with_device_helper -- --ignored --exact tests::copy_file_over_dbus_with_device_helper"]
    async fn copy_file_over_dbus_with_device_helper() {
        use tokio::io::AsyncReadExt;

        let binary = PathBuf::from(std::env::var_os("ADBSHARE_TEST_PROXY_BIN")
            .expect("set ADBSHARE_TEST_PROXY_BIN to the host-built adbshare-proxy binary"));
        assert!(binary.is_absolute() && binary.is_file(), "ADBSHARE_TEST_PROXY_BIN must be an absolute path to a host-built helper");
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").expect("run this test under dbus-run-session");
        let mut helper = TestHelper::new();
        let port = allocate_host_port().unwrap();
        helper.child = Some(std::process::Command::new(binary)
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn().expect("start host-built device helper"));
        let client = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(helper.child.as_mut().unwrap().try_wait().unwrap().is_none(), "helper exited before becoming ready");
                if let Ok(client) = ProxyClient::connect(format!("127.0.0.1:{port}"), 1).await {
                    return client;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("helper did not start within 5 seconds");
        assert_eq!(client.max_conns(), 1);

        tokio::time::timeout(Duration::from_secs(15), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let disconnected = ProxyClient::connect(listener.local_addr().unwrap().to_string(), 1).await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let state = Arc::new(Mutex::new(State::default()));
            for (serial, client) in [("test-helper", client), ("test-disconnect", disconnected)] {
                state.lock().devices.insert(DeviceId(serial.into()), DeviceSlot {
                    mountpoint: None,
                    client: Arc::new(client),
                    host_port: port,
                    setup_ok: true,
                });
            }
            let (queue, _rx) = JobQueue::new(1);
            let service = ConnectionBuilder::session().unwrap()
                .serve_at("/org/adbshare/Manager", ManagerInterface { state, queue }).unwrap()
                .build().await.unwrap();
            let connection = zbus::Connection::session().await.unwrap();
            let proxy = zbus::Proxy::new(
                &connection,
                service.unique_name().unwrap().as_str(),
                "/org/adbshare/Manager",
                "org.adbshare.Manager",
            ).await.unwrap();
            let source = helper.dir.join("source");
            let destination = helper.dir.join("destination");
            let existing = helper.dir.join("existing");
            let second = helper.dir.join("second");
            let data: Vec<u8> = (0..1024 * 1024 + 19).map(|i| (i % 251) as u8).collect();
            std::fs::write(&source, &data).unwrap();
            std::fs::write(&existing, b"keep existing destination").unwrap();
            let source_path = source.to_str().unwrap();
            let result: zbus::Result<()> = proxy.call("CopyFile", &("test-helper", source_path, destination.to_str().unwrap())).await;
            result.unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), data);
            assert_eq!(std::fs::read(&destination).unwrap(), data);

            let result: zbus::Result<()> = proxy.call("CopyFile", &("test-helper", source_path, existing.to_str().unwrap())).await;
            match result.unwrap_err() {
                zbus::Error::MethodError(name, Some(message), _) => {
                    assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.Failed");
                    assert!(message.contains("server error: Exists"), "{message}");
                    assert!(!message.contains("completion unknown"), "{message}");
                }
                error => panic!("unexpected D-Bus error: {error}"),
            }
            assert_eq!(std::fs::read(&existing).unwrap(), b"keep existing destination");
            assert_eq!(std::fs::read(&source).unwrap(), data);
            let result: zbus::Result<()> = proxy.call("CopyFile", &("test-helper", source_path, second.to_str().unwrap())).await;
            result.unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), data);
            assert_eq!(std::fs::read(&second).unwrap(), data);
            assert_eq!(std::fs::read(&destination).unwrap(), data);

            let disconnect = async move {
                let mut header = [0; 5];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header[0], adb_proxy::ops::Op::CopyFile as u8);
                let mut args = vec![0; u32::from_le_bytes(header[1..].try_into().unwrap()) as usize];
                stream.read_exact(&mut args).await.unwrap();
                drop(stream);
            };
            let args = ("test-disconnect", source_path, existing.to_str().unwrap());
            let call = proxy.call::<_, _, ()>("CopyFile", &args);
            let (result, ()) = tokio::join!(call, disconnect);
            match result.unwrap_err() {
                zbus::Error::MethodError(name, Some(message), _) => {
                    assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.Failed");
                    assert_eq!(message, "copy_file: connection closed; completion unknown; destination may be incomplete or still copying");
                }
                error => panic!("unexpected D-Bus error: {error}"),
            }
            assert!(tokio::time::timeout(Duration::from_millis(100), listener.accept()).await.is_err());
            assert_eq!(std::fs::read(&source).unwrap(), data);
            assert_eq!(std::fs::read(&existing).unwrap(), b"keep existing destination");
        }).await.expect("D-Bus copy integration test timed out");
    }

    fn tree_stat(mode: u32, size: u64) -> adb_proxy::Stat {
        adb_proxy::Stat {
            mode: adb_proxy::FileMode(mode),
            size,
            mtime: 0,
            atime: 0,
            ctime: 0,
            uid: 0,
            gid: 0,
            nlink: 1,
            blksize: 4096,
            blocks: 0,
        }
    }

    async fn mock_tree_server(entries: std::collections::HashMap<String, Vec<(String, adb_proxy::Stat)>>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            loop {
                let mut header = [0u8; 5];
                if stream.read_exact(&mut header).await.is_err() { return; }
                let len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
                let mut args = vec![0u8; len];
                if stream.read_exact(&mut args).await.is_err() { return; }
                let response = match header[0] {
                    0x07 | 0x12 => vec![0u8],
                    0x06 => {
                        let path_len = u32::from_le_bytes(args[0..4].try_into().unwrap()) as usize;
                        let path = String::from_utf8_lossy(&args[4..4 + path_len]).into_owned();
                        let mut body = vec![0u8];
                        if let Some(list) = entries.get(&path) {
                            for (name, stat) in list {
                                body.extend_from_slice(&(name.len() as u32).to_le_bytes());
                                body.extend_from_slice(name.as_bytes());
                                body.extend_from_slice(&stat.encode());
                            }
                        }
                        body
                    }
                    _ => {
                        let mut body = vec![0x0Bu8];
                        body.extend_from_slice(b"unsupported");
                        let mut framed = (body.len() as u32).to_le_bytes().to_vec();
                        framed.extend_from_slice(&body);
                        if stream.write_all(&framed).await.is_err() { return; }
                        continue;
                    }
                };
                let mut framed = (response.len() as u32).to_le_bytes().to_vec();
                framed.extend_from_slice(&response);
                if stream.write_all(&framed).await.is_err() { return; }
            }
        });
        (addr, handle)
    }

    fn tree_test_manager(client: ProxyClient) -> ManagerInterface {
        let (queue, _rx) = JobQueue::new(4);
        let state = Arc::new(Mutex::new(State::default()));
        state.lock().devices.insert(
            DeviceId("mock".into()),
            DeviceSlot { mountpoint: None, client: Arc::new(client), host_port: 0, setup_ok: true },
        );
        ManagerInterface { state, queue }
    }

    #[tokio::test]
    async fn tree_pull_enqueues_files_skips_symlinks_and_creates_dirs() {
        let mut entries = std::collections::HashMap::new();
        entries.insert("/".to_string(), vec![
            ("sub".to_string(), tree_stat(0o040755, 0)),
            ("a.txt".to_string(), tree_stat(0o100644, 10)),
            ("link".to_string(), tree_stat(0o120777, 0)),
        ]);
        entries.insert("/sub".to_string(), vec![
            ("b.txt".to_string(), tree_stat(0o100644, 20)),
            ("empty".to_string(), tree_stat(0o040755, 0)),
        ]);
        entries.insert("/sub/empty".to_string(), vec![]);
        let (addr, server) = mock_tree_server(entries).await;
        let client = tokio::time::timeout(Duration::from_secs(5), ProxyClient::connect(addr, 1))
            .await.expect("connect").unwrap();
        let manager = tree_test_manager(client);
        let base = std::env::temp_dir().join(format!(
            "adbshare-tree-pull-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let json = tokio::time::timeout(Duration::from_secs(15), manager.enqueue_tree_pull(
            "mock", "/", base.to_str().unwrap(), "skip", false)).await.expect("timeout").unwrap();
        let result: TreeEnqueueResult = serde_json::from_str(&json).unwrap();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.enqueued.len(), 2);
        assert!(base.join("sub").is_dir());
        assert!(base.join("sub").join("empty").is_dir());
        assert!(!base.join("link").exists());
        let jobs = manager.queue.jobs_snapshot();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|j| j.device.as_deref() == Some("mock")));
        let _ = std::fs::remove_dir_all(&base);
        server.abort();
    }

    #[tokio::test]
    async fn tree_push_walks_local_tree_and_enqueues_push_jobs() {
        let (addr, server) = mock_tree_server(std::collections::HashMap::new()).await;
        let client = tokio::time::timeout(Duration::from_secs(5), ProxyClient::connect(addr, 1))
            .await.expect("connect").unwrap();
        let manager = tree_test_manager(client);
        let base = std::env::temp_dir().join(format!(
            "adbshare-tree-push-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.txt"), b"a").unwrap();
        std::fs::write(base.join("sub").join("b.txt"), b"b").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", base.join("link")).unwrap();
        let json = tokio::time::timeout(Duration::from_secs(15), manager.enqueue_tree_push(
            "mock", base.to_str().unwrap(), "/dst", "keep-both", true)).await.expect("timeout").unwrap();
        let result: TreeEnqueueResult = serde_json::from_str(&json).unwrap();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.enqueued.len(), 2);
        let jobs = manager.queue.jobs_snapshot();
        assert_eq!(jobs.len(), 2);
        for job in &jobs {
            assert!(matches!(job.options.overwrite, transfer_engine::job::OverwriteMode::Rename));
            assert!(matches!(job.options.verify, transfer_engine::job::VerifyMode::On));
            assert!(job.destination.to_string_lossy().starts_with("/dst"));
        }
        let _ = std::fs::remove_dir_all(&base);
        server.abort();
    }

    #[tokio::test]
    async fn copy_tree_recurses_and_skips_symlinks() {
        let mut entries = std::collections::HashMap::new();
        entries.insert("/src".to_string(), vec![
            ("sub".to_string(), tree_stat(0o040755, 0)),
            ("a.txt".to_string(), tree_stat(0o100644, 5)),
            ("link".to_string(), tree_stat(0o120777, 0)),
        ]);
        entries.insert("/src/sub".to_string(), vec![
            ("b.txt".to_string(), tree_stat(0o100644, 6)),
        ]);
        let (addr, server) = mock_tree_server(entries).await;
        let client = tokio::time::timeout(Duration::from_secs(5), ProxyClient::connect(addr, 1))
            .await.expect("connect").unwrap();
        let manager = tree_test_manager(client);
        let json = tokio::time::timeout(Duration::from_secs(15),
            manager.copy_tree("mock", "/src", "/dst")).await.expect("timeout").unwrap();
        let result: TreeEnqueueResult = serde_json::from_str(&json).unwrap();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.enqueued.len(), 2);
        assert!(manager.copy_tree("mock", "/src", "/src").await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn diagnostics_reports_host_facts_as_json() {
        let (queue, _rx) = JobQueue::new(1);
        let manager = ManagerInterface {
            state: Arc::new(Mutex::new(State {
                adb_server: "127.0.0.1:5037".into(),
                mount_base: PathBuf::from("/tmp/adbshare-test"),
                proxy_conns: 4,
                ..State::default()
            })),
            queue,
        };
        let json = manager.diagnostics().await.unwrap();
        let report: DiagnosticReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report.adb_server, "127.0.0.1:5037");
        assert_eq!(report.mount_base, "/tmp/adbshare-test");
        assert_eq!(report.proxy_conns, 4);
        assert!(report.devices.is_empty());
    }

    #[test]
    fn job_options_parse_skip_replace_keep_both_and_verify() {
        let skip = parse_job_options("skip", false).unwrap();
        assert!(matches!(skip.overwrite, transfer_engine::job::OverwriteMode::SkipExisting));
        assert!(matches!(skip.verify, transfer_engine::job::VerifyMode::Off));
        let replace = parse_job_options("replace", true).unwrap();
        assert!(matches!(replace.overwrite, transfer_engine::job::OverwriteMode::Always));
        assert!(matches!(replace.verify, transfer_engine::job::VerifyMode::On));
        let keep = parse_job_options("keep-both", false).unwrap();
        assert!(matches!(keep.overwrite, transfer_engine::job::OverwriteMode::Rename));
        assert!(parse_job_options("overwrite", false).is_err());
    }

    #[tokio::test]
    async fn copy_file_validates_paths_and_requires_connected_device() {
        let (queue, _rx) = JobQueue::new(1);
        let manager = ManagerInterface {
            state: Arc::new(Mutex::new(State::default())),
            queue,
        };
        for path in ["", "relative", "/nul\0hidden", &"/".repeat(4097)] {
            for (src, dst) in [(path, "/destination"), ("/source", path)] {
                assert!(matches!(manager.copy_file("missing", src, dst).await,
                    Err(zbus::fdo::Error::InvalidArgs(_))));
            }
        }
        assert!(matches!(manager.copy_file("missing", "/source", "/destination").await,
            Err(zbus::fdo::Error::ServiceUnknown(_))));
    }

    #[test]
    fn adb_server_parses_ipv4_and_default() {
        assert_eq!(parse_adb_server("127.0.0.1:5037"), ("127.0.0.1".into(), 5037));
        assert_eq!(parse_adb_server("127.0.0.1:5555"), ("127.0.0.1".into(), 5555));
        assert_eq!(parse_adb_server("localhost"), ("localhost".into(), 5037));
        assert_eq!(parse_adb_server("localhost:abc"), ("localhost".into(), 5037));
    }

    #[test]
    fn adb_server_parses_bracketed_ipv6() {
        assert_eq!(parse_adb_server("[::1]:5037"), ("::1".into(), 5037));
        assert_eq!(parse_adb_server("[::1]:5555"), ("::1".into(), 5555));
        assert_eq!(parse_adb_server("[fe80::1]"), ("fe80::1".into(), 5037));
    }

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
    // `--proxy-conns 0` would create a zero-permit semaphore that deadlocks
    // every RPC; refuse it (and absurd values) with a clear error.
    if !(1..=64).contains(&cli.proxy_conns) {
        anyhow::bail!(
            "--proxy-conns must be between 1 and 64 (got {})",
            cli.proxy_conns
        );
    }
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

    let state = Arc::new(Mutex::new(State {
        adb_server: cli.adb_server.clone(),
        mount_base: mount_base.clone(),
        proxy_conns: cli.proxy_conns,
        no_fuse: cli.no_fuse,
        ..State::default()
    }));
    let (queue, mut queue_rx) = JobQueue::new(transfer_engine::DEFAULT_PARALLELISM);
    let (adb_host, adb_port) = parse_adb_server(&cli.adb_server);
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
                    ensure_device_ready(&state_clone, &queue_for_setup, &id, &mount_base_clone, no_fuse, proxy_conns)
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
                    ensure_device_ready(&state_clone, &queue_for_setup, &id, &mount_base_clone, no_fuse, proxy_conns)
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

/// Maximum recursion depth for `delete_recursive`; beyond this we assume a
/// cycle or a pathological tree and refuse rather than recurse forever.
const DELETE_MAX_DEPTH: u32 = 64;

/// Recursively delete `path` on the device via proxy ops (there is no
/// server-side `rm -r` in the proxy protocol). `depth` is capped at
/// `DELETE_MAX_DEPTH`.
async fn delete_recursive(client: &ProxyClient, path: &str, depth: u32) -> anyhow::Result<()> {
    if depth > DELETE_MAX_DEPTH {
        anyhow::bail!("delete: recursion deeper than {DELETE_MAX_DEPTH} levels at '{path}'");
    }
    let st = client.lstat(path).await?;
    if st.mode.is_symlink() || !st.mode.is_dir() {
        return client.unlink(path).await.map_err(|e| anyhow::anyhow!("{e}"));
    }
    for entry in client.listdir(path).await? {
        let child = format!("{}/{}", path.trim_end_matches('/'), entry.name);
        Box::pin(delete_recursive(client, &child, depth + 1)).await?;
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

    /// Storage insight: sizes of the immediate children of `path` on the
    /// device plus filesystem totals (via the `DISKUSAGE`/`statvfs` proxy
    /// op). Returns JSON `DuResult`; `entries` is the `(name, size)` array
    /// (stat size per entry; directories report their entry size, not
    /// recursive totals).
    async fn du(&self, device: &str, path: &str) -> zbus::fdo::Result<String> {
        let client = client_for(&self.state, device)?;
        let usage = client.disk_usage(path).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("du: {e}")))?;
        let entries = match client.listdir(path).await {
            Ok(list) => list
                .into_iter()
                .map(|e| DuEntry { name: e.name, size: e.stat.size })
                .collect(),
            Err(_) => {
                // `path` may be a file: report it as a single entry.
                let st = client.stat(path).await
                    .map_err(|e| zbus::fdo::Error::Failed(format!("du: {e}")))?;
                let name = path.rsplit('/').next().unwrap_or(path).to_string();
                vec![DuEntry { name, size: st.size }]
            }
        };
        let out = DuResult {
            path: path.to_string(),
            avail_bytes: usage.avail_bytes,
            total_bytes: usage.total_bytes,
            entries,
        };
        serde_json::to_string(&out)
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
        if !local_dir.starts_with('/') || !device_dir.starts_with('/') {
            return Err(zbus::fdo::Error::InvalidArgs(
                "tree paths must be absolute".into(),
            ));
        }
        let local_base = PathBuf::from(local_dir);
        let device_base = PathBuf::from(device_dir);
        let client = client_for(&self.state, device)?;
        let mut out = TreeEnqueueResult::default();
        let mut stack = vec![(local_base.clone(), device_base.clone(), 0u32)];
        while let Some((local, remote, depth)) = stack.pop() {
            if depth > 32 {
                out.errors.push(format!("{}: directory nesting too deep", local.display()));
                continue;
            }
            let read = std::fs::read_dir(&local)
                .map_err(|e| zbus::fdo::Error::Failed(format!("read {}: {e}", local.display())))?;
            for entry in read {
                let entry = entry
                    .map_err(|e| zbus::fdo::Error::Failed(format!("read {}: {e}", local.display())))?;
                if out.enqueued.len() > 10_000 {
                    out.errors.push("too many files (limit 10000)".into());
                    return serde_json::to_string(&out)
                        .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")));
                }
                let file_type = entry
                    .file_type()
                    .map_err(|e| zbus::fdo::Error::Failed(format!("stat {}: {e}", entry.path().display())))?;
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
        serde_json::to_string(&out)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
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
        if !device_dir.starts_with('/') || !local_dir.starts_with('/') {
            return Err(zbus::fdo::Error::InvalidArgs(
                "tree paths must be absolute".into(),
            ));
        }
        let local_base = PathBuf::from(local_dir);
        std::fs::create_dir_all(&local_base)
            .map_err(|e| zbus::fdo::Error::Failed(format!("mkdir {}: {e}", local_base.display())))?;
        let client = client_for(&self.state, device)?;
        let mut out = TreeEnqueueResult::default();
        let mut stack = vec![(device_dir.to_string(), local_base, 0u32)];
        while let Some((remote, local, depth)) = stack.pop() {
            if depth > 32 {
                out.errors.push(format!("{remote}: directory nesting too deep"));
                continue;
            }
            let entries = client.listdir(&remote).await
                .map_err(|e| zbus::fdo::Error::Failed(format!("list {remote}: {e}")))?;
            for entry in entries {
                if out.enqueued.len() > 10_000 {
                    out.errors.push("too many files (limit 10000)".into());
                    return serde_json::to_string(&out)
                        .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")));
                }
                if entry.stat.mode.is_symlink() {
                    continue;
                }
                let remote_child = format!("{}/{}", remote.trim_end_matches('/'), entry.name);
                let local_child = local.join(&entry.name);
                if entry.stat.mode.is_dir() {
                    std::fs::create_dir_all(&local_child)
                        .map_err(|e| zbus::fdo::Error::Failed(format!("mkdir {}: {e}", local_child.display())))?;
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
        serde_json::to_string(&out)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    async fn copy_tree(&self, device: &str, src_dir: &str, dst_dir: &str) -> zbus::fdo::Result<String> {
        for path in [src_dir, dst_dir] {
            if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "copy paths must be absolute, non-NUL, and at most 4096 bytes".into(),
                ));
            }
        }
        if src_dir == dst_dir {
            return Err(zbus::fdo::Error::InvalidArgs("source and destination are the same".into()));
        }
        let client = client_for(&self.state, device)?;
        let mut out = TreeEnqueueResult::default();
        let mut stack = vec![(src_dir.to_string(), dst_dir.to_string(), 0u32)];
        while let Some((src, dst, depth)) = stack.pop() {
            if depth > 32 {
                out.errors.push(format!("{src}: directory nesting too deep"));
                continue;
            }
            if out.enqueued.len() > 10_000 {
                out.errors.push("too many files (limit 10000)".into());
                break;
            }
            let _ = client.mkdir(&dst, 0o755).await;
            let entries = client.listdir(&src).await
                .map_err(|e| zbus::fdo::Error::Failed(format!("list {src}: {e}")))?;
            for entry in entries {
                if entry.stat.mode.is_symlink() {
                    continue;
                }
                let src_child = format!("{}/{}", src.trim_end_matches('/'), entry.name);
                let dst_child = format!("{}/{}", dst.trim_end_matches('/'), entry.name);
                if entry.stat.mode.is_dir() {
                    stack.push((src_child, dst_child, depth + 1));
                } else {
                    match client.copy_file(&src_child, &dst_child).await {
                        Ok(()) => out.enqueued.push(out.enqueued.len() as u64 + 1),
                        Err(e) => out.errors.push(format!("copy {src_child}: {e}")),
                    }
                }
            }
        }
        serde_json::to_string(&out)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    /// Photo import (backend only; no GUI button yet). Lists `src_dirs`
    /// (defaults to `/sdcard/DCIM/Camera` when empty), skips files already
    /// present under `dest_base/YYYY-MM-DD/<name>` with the same size, and
    /// enqueues `Pull` jobs for the rest. Returns a JSON
    /// `transfer_engine::PhotoImportResult`.
    async fn import_photos(
        &self,
        device: &str,
        src_dirs: Vec<String>,
        dest_base: &str,
    ) -> zbus::fdo::Result<String> {
        let dest = PathBuf::from(dest_base);
        if !dest.is_absolute() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "dest_base must be an absolute path".into(),
            ));
        }
        let client = client_for(&self.state, device)?;
        let result = transfer_engine::import_photos(
            &client,
            &self.queue,
            device,
            &src_dirs,
            &dest,
        )
        .await
        .map_err(|e| zbus::fdo::Error::Failed(format!("import_photos: {e}")))?;
        serde_json::to_string(&result)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    /// Mirror diff (no auto-sync): list `remote_path` on the device and
    /// return a JSON array of `transfer_engine::MirrorEntry` (regular files
    /// only) so the GUI can call `plan_mirror(local_dir, remote)` itself and
    /// enqueue push/pull jobs via `enqueue_push`/`enqueue_pull`.
    async fn mirror_diff(&self, device: &str, remote_path: &str) -> zbus::fdo::Result<String> {
        let client = client_for(&self.state, device)?;
        let entries = client.listdir(remote_path).await
            .map_err(|e| zbus::fdo::Error::Failed(format!("mirror_diff: {e}")))?;
        let out: Vec<transfer_engine::MirrorEntry> = entries
            .into_iter()
            .filter(|e| !e.stat.mode.is_dir())
            .map(|e| transfer_engine::MirrorEntry { name: e.name, size: e.stat.size })
            .collect();
        serde_json::to_string(&out)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
    }

    async fn diagnostics(&self) -> zbus::fdo::Result<String> {
        let (devices, adb_server, mount_base, proxy_conns, no_fuse) = {
            let state = self.state.lock();
            let devices = state.devices.iter().map(|(id, slot)| DeviceDiag {
                serial: id.0.clone(),
                setup_ok: slot.setup_ok,
                mounted: slot.mountpoint.is_some(),
            }).collect();
            (devices, state.adb_server.clone(), state.mount_base.clone(), state.proxy_conns, state.no_fuse)
        };
        let adb_output = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new("adb").arg("version").kill_on_drop(true).output(),
        ).await;
        let (adb_ok, adb_version) = match adb_output {
            Ok(Ok(out)) if out.status.success() => {
                let first_line = String::from_utf8_lossy(&out.stdout)
                    .lines().next().unwrap_or("").to_string();
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
            devices,
        };
        serde_json::to_string(&report)
            .map_err(|e| zbus::fdo::Error::Failed(format!("serialize: {e}")))
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

    /// Requeue every failed job as pending so the worker loop retries it.
    /// Returns the number of jobs requeued.
    async fn retry_failed(&self) -> zbus::fdo::Result<u64> {
        Ok(self.queue.retry_failed())
    }

    async fn retry_job(&self, id: u64) -> zbus::fdo::Result<bool> {
        Ok(self.queue.retry_job(id))
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

    async fn copy_file(&self, device: &str, src: &str, dst: &str) -> zbus::fdo::Result<()> {
        for path in [src, dst] {
            if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "copy paths must be absolute, non-NUL, and at most 4096 bytes".into(),
                ));
            }
        }
        let client = client_for(&self.state, device)?;
        client.copy_file(src, dst).await
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
        delete_recursive(&client, path, 0).await
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

    /// `adb install -r` with a 120s timeout. Supports both host-local APK paths
    /// and device-local paths (e.g. `/sdcard/...` or `/storage/...`).
    async fn install_apk(&self, device: &str, path: &str) -> zbus::fdo::Result<String> {
        if device.is_empty() || path.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs("device and path are required".into()));
        }

        let is_device_path = path.starts_with("/sdcard/")
            || path.starts_with("/storage/")
            || path.starts_with("/data/")
            || !std::path::Path::new(path).exists();

        let cmd = if is_device_path {
            if path.starts_with("/data/local/tmp/") {
                let escaped_path = path.replace('\'', "'\\''");
                Command::new("adb")
                    .args(["-s", device, "shell", &format!("pm install -r '{escaped_path}'")])
                    .kill_on_drop(true)
                    .output()
            } else {
                let escaped_path = path.replace('\'', "'\\''");
                let script = format!(
                    "tmp=\"/data/local/tmp/adbshare_$$.apk\" && cp '{escaped_path}' \"$tmp\" && pm install -r \"$tmp\"; res=$?; rm -f \"$tmp\"; exit $res"
                );
                Command::new("adb")
                    .args(["-s", device, "shell", &script])
                    .kill_on_drop(true)
                    .output()
            }
        } else {
            Command::new("adb")
                .args(["-s", device, "install", "-r", path])
                .kill_on_drop(true)
                .output()
        };

        let out = tokio::time::timeout(Duration::from_secs(120), cmd)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("Installation timed out after 120s".into()))?
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to execute adb: {e}")))?;

        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let trimmed = text.trim();
        let is_failure = !out.status.success()
            || trimmed.contains("Failure [")
            || trimmed.starts_with("Failure")
            || trimmed.contains("INSTALL_FAILED");

        if is_failure {
            let user_msg = if trimmed.contains("INSTALL_FAILED_VERSION_DOWNGRADE") {
                "Cannot install: a newer version of this application is already installed on the device."
            } else if trimmed.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") {
                "Cannot install: signatures do not match the installed version. Please uninstall the existing app first."
            } else if trimmed.contains("INSTALL_FAILED_INSUFFICIENT_STORAGE") {
                "Cannot install: device has insufficient storage space."
            } else if trimmed.contains("INSTALL_FAILED_CONFLICTING_PROVIDER") {
                "Cannot install: conflicting content provider with an existing application."
            } else if trimmed.contains("INSTALL_FAILED_NO_MATCHING_ABIS") {
                "Cannot install: APK is incompatible with this device's CPU architecture."
            } else if trimmed.contains("INSTALL_PARSE_FAILED") {
                "Cannot install: corrupted or invalid APK file."
            } else {
                trimmed
            };
            return Err(zbus::fdo::Error::Failed(format!("{user_msg}\n\n{trimmed}")));
        }

        Ok(trimmed.to_string())
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
struct DuEntry {
    name: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DuResult {
    path: String,
    avail_bytes: u64,
    total_bytes: u64,
    entries: Vec<DuEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceDiag {
    serial: String,
    setup_ok: bool,
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
    devices: Vec<DeviceDiag>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TreeEnqueueResult {
    #[serde(default)]
    enqueued: Vec<u64>,
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