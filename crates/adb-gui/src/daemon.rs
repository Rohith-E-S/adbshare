//! D-Bus client for `adb-daemon`.
//!
//! GPUI runs on its own executor, which is not tokio, and zbus needs a tokio
//! reactor. So a single-threaded tokio runtime is pinned to a dedicated
//! thread (the same trick the GTK build used) and every call is dispatched onto
//! it, with the result handed back to the GPUI task through an
//! [`async_channel`]. GPUI's task waker drives the reply, so the UI thread never
//! blocks on D-Bus.

use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::*;
use zbus::names::WellKnownName;

use crate::protocol::{DeviceInfoDto, DirEntry, DirEntryDto, JobDto, JobInfo, TransferPolicy};

/// Bus name, object path and interface the daemon serves.
const SERVICE: &str = "org.adbshare.Manager";
const PATH: &str = "/org/adbshare/Manager";

#[zbus::proxy(
    default_service = "org.adbshare.Manager",
    interface = "org.adbshare.Manager",
    default_path = "/org/adbshare/Manager"
)]
pub trait Manager {
    async fn list_devices(&self) -> zbus::Result<Vec<String>>;
    async fn device_info(&self, serial: &str) -> zbus::Result<String>;
    async fn list_dir(&self, device: &str, path: &str) -> zbus::Result<String>;
    async fn enqueue_push(
        &self,
        device: &str,
        local_path: &str,
        device_path: &str,
    ) -> zbus::Result<u64>;
    async fn enqueue_pull(
        &self,
        device: &str,
        device_path: &str,
        local_path: &str,
    ) -> zbus::Result<u64>;
    async fn list_jobs(&self) -> zbus::Result<String>;
    async fn pause_job(&self, id: u64) -> zbus::Result<bool>;
    async fn resume_job(&self, id: u64) -> zbus::Result<bool>;
    async fn cancel_job(&self, id: u64) -> zbus::Result<bool>;
    async fn retry_failed(&self) -> zbus::Result<u64>;
    async fn enqueue_tree_push(
        &self,
        device: &str,
        local_dir: &str,
        device_dir: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::Result<String>;
    async fn enqueue_tree_pull(
        &self,
        device: &str,
        device_dir: &str,
        local_dir: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::Result<String>;
    async fn copy_tree(&self, device: &str, src_dir: &str, dst_dir: &str) -> zbus::Result<String>;
    async fn diagnostics(&self) -> zbus::Result<String>;
    async fn pair_wireless(&self, address: &str, code: &str) -> zbus::Result<String>;
    async fn enqueue_push_with_options(
        &self,
        device: &str,
        local_path: &str,
        device_path: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::Result<u64>;
    async fn enqueue_pull_with_options(
        &self,
        device: &str,
        device_path: &str,
        local_path: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::Result<u64>;
    async fn mkdir(&self, device: &str, path: &str) -> zbus::Result<()>;
    async fn rename(&self, device: &str, src: &str, dst: &str) -> zbus::Result<()>;
    async fn copy_file(&self, device: &str, src: &str, dst: &str) -> zbus::Result<()>;
    async fn delete(&self, device: &str, path: &str) -> zbus::Result<()>;
    async fn connect_wireless(&self, address: &str) -> zbus::Result<String>;
    async fn mountpoint_for(&self, device: &str) -> zbus::Result<String>;
    async fn install_apk(&self, device: &str, path: &str) -> zbus::Result<String>;
}

/// Handle to the pinned tokio runtime.
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// The daemon proxy, created once on the tokio runtime.
static MANAGER: tokio::sync::OnceCell<ManagerProxy<'static>> = tokio::sync::OnceCell::const_new();

/// Spin up the tokio runtime thread. Called once from `main` before the window
/// opens, so the first D-Bus call does not pay for thread startup.
pub fn start(_cx: &mut gpui::App) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!("failed to build the D-Bus runtime: {err}");
            return;
        }
    };

    let handle = runtime.handle().clone();
    let spawned = std::thread::Builder::new()
        .name("adbshare-tokio".into())
        .spawn(move || {
            runtime.block_on(async { std::future::pending::<()>().await });
        });

    match spawned {
        Ok(_) => {
            let _ = RUNTIME.set(handle);
        }
        Err(err) => tracing::error!("failed to start the D-Bus thread: {err}"),
    }
}

/// Run an arbitrary async block on the pinned tokio runtime.
///
/// Used by callers other than the D-Bus client, such as the portal file chooser
/// in [`crate::filechooser`], which needs the same reactor zbus provides.
pub async fn on_tokio<F, Fut, T>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    let handle = runtime().map_err(|err| err.to_string())?;
    let (tx, rx) = async_channel::bounded(1);
    handle.spawn(async move {
        let _ = tx.send(f().await).await;
    });
    rx.recv()
        .await
        .unwrap_or_else(|_| Err("the task ended without a reply".into()))
}

fn runtime() -> anyhow::Result<&'static tokio::runtime::Handle> {
    RUNTIME
        .get()
        .ok_or_else(|| anyhow::anyhow!("the D-Bus runtime is not running"))
}

/// Run `f` on the tokio runtime and await its result from a GPUI task.
///
/// The returned future is polled by GPUI's executor, so this is safe to await
/// from `cx.spawn`: the reply wakes the GPUI task and the UI thread never
/// blocks on D-Bus. Both bus errors and JSON decoding failures come back as
/// `String`, which is what the views render in an error toast or dialog.
async fn dispatch<T, F, Fut>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
{
    let handle = runtime().map_err(|err| err.to_string())?;
    let (tx, rx) = async_channel::bounded(1);
    handle.spawn(async move {
        let _ = tx.send(f().await.map_err(|err| err.to_string())).await;
    });
    rx.recv()
        .await
        .unwrap_or_else(|_| Err("the D-Bus task ended without a reply".into()))
}

/// Turn a D-Bus failure into something a person can act on.
///
/// The raw errors are developer-facing — `org.freedesktop.DBus.Error.
/// ServiceUnknown: The name is not activatable` says nothing to someone whose
/// daemon simply is not running, and the common causes have different fixes.
pub fn explain(err: &str) -> String {
    if err.contains("ServiceUnknown") || err.contains("was not provided by any .service files") {
        return "The daemon is not running and could not be started automatically. \
                Start it yourself with `adb-daemon`, or check that it is on PATH."
            .to_string();
    }
    if err.contains("NameHasNoOwner") {
        return "The daemon is not running. Start it with `adb-daemon`.".to_string();
    }
    if err.contains("AccessDenied") || err.contains("not allowed") {
        return "The session bus refused the connection. Check that you are in a desktop \
                session and that the bus policy allows it."
            .to_string();
    }
    if err.contains("timed out") || err.contains("Timeout") {
        return "The daemon did not answer in time. It may be busy or stuck; check its log."
            .to_string();
    }
    if err.contains("D-Bus runtime") {
        return "The D-Bus client could not start.".to_string();
    }
    // Anything else is passed through: a specific error is usually the useful
    // one, and the reason is shown verbatim in the sidebar.
    err.to_string()
}

async fn get_manager() -> anyhow::Result<&'static ManagerProxy<'static>> {
    MANAGER
        .get_or_try_init(|| async {
            // D-Bus activation is the primary path (the .service file plus the
            // systemd user unit). When those are not installed — a `cargo run`
            // from the source tree, say — fall back to spawning the daemon.
            ensure_daemon_running().await;
            let conn = zbus::Connection::session().await?;
            let proxy = ManagerProxy::builder(&conn)
                .destination(WellKnownName::try_from(SERVICE)?)?
                .build()
                .await?;
            tracing::debug!("connected to {SERVICE} at {PATH}");
            Ok(proxy)
        })
        .await
}

/// Best-effort daemon startup for environments without D-Bus activation.
/// No-op when the name is already owned, which is the normal packaged case.
async fn ensure_daemon_running() {
    // Fast path: the daemon already owns the bus name.
    if let Ok(conn) = zbus::Connection::session().await
        && let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await
        && let Ok(name) = WellKnownName::try_from(SERVICE)
        && dbus.name_has_owner(name.into()).await.unwrap_or(false)
    {
        return;
    }

    let Some(exe) = daemon_binary_path() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // Give the daemon a moment to claim the bus name before the first call.
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

/// Locate `adb-daemon`: next to `adb-gui` first (dev runs and the release
/// tarball), then on `PATH`. `None` if neither resolves; the caller then skips
/// the spawn.
fn daemon_binary_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let next_to = dir.join("adb-daemon");
        if next_to.is_file() {
            return Some(next_to);
        }
    }
    daemon_on_path(std::env::var_os("PATH").as_deref())
}

/// Scan a `PATH`-shaped list for an `adb-daemon` file.
fn daemon_on_path(paths: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let paths = paths?;
    std::env::split_paths(&paths)
        .map(|d| d.join("adb-daemon"))
        .find(|p| p.is_file())
}

// ── Devices ──────────────────────────────────────────────────────────────────

/// Every serial the daemon currently sees, authorized or not.
pub async fn list_devices() -> Result<Vec<String>, String> {
    dispatch(|| async {
        let proxy = get_manager().await?;
        Ok(proxy.list_devices().await?)
    })
    .await
}

/// Live metadata for one device, decoded from the daemon's JSON payload.
pub async fn device_info(serial: &str) -> Result<DeviceInfoDto, String> {
    let serial = serial.to_string();
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let json = proxy.device_info(&serial).await?;
        Ok(serde_json::from_str(&json)?)
    })
    .await
}

/// The FUSE mountpoint for a device, or an empty string when FUSE is disabled.
pub async fn mountpoint_for(device: &str) -> Result<String, String> {
    let device = device.to_string();
    dispatch(move || async move {
        let proxy = get_manager().await?;
        Ok(proxy.mountpoint_for(&device).await?)
    })
    .await
}

// ── Browsing ─────────────────────────────────────────────────────────────────

/// List one directory on a device.
pub async fn list_dir(device: &str, path: &str) -> Result<Vec<DirEntry>, String> {
    let (device, path) = (device.to_string(), path.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let json = proxy.list_dir(&device, &path).await?;
        let entries: Vec<DirEntryDto> = serde_json::from_str(&json)?;
        Ok(entries.into_iter().map(DirEntry::from).collect())
    })
    .await
}

// ── Transfers ────────────────────────────────────────────────────────────────

/// Conflict policy and verification flag to apply to a new transfer.
///
/// The GTK build read these from two `OnceLock<Mutex<..>>` statics. The values
/// live in a [`TransferPolicy`] global now, but the calls that need them run on
/// the tokio runtime where there is no `App`, so the caller reads the global and
/// passes the pair in. See [`transfer_policy`].
pub type Policy = (String, bool);

/// Queue a local file or folder for upload, falling back to the legacy call
/// when the daemon predates `*_with_options`.
pub async fn enqueue_push(
    device: &str,
    local: &str,
    device_path: &str,
    policy: Policy,
) -> Result<u64, String> {
    let (device, local, device_path) = (
        device.to_string(),
        local.to_string(),
        device_path.to_string(),
    );
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let (overwrite, verify) = policy;
        match proxy
            .enqueue_push_with_options(&device, &local, &device_path, &overwrite, verify)
            .await
        {
            Ok(id) => Ok(id),
            Err(err) if is_unknown_method(&err) => {
                Ok(proxy.enqueue_push(&device, &local, &device_path).await?)
            }
            Err(err) => Err(err.into()),
        }
    })
    .await
}

/// Queue a device file or folder for download, with the same legacy fallback.
pub async fn enqueue_pull(
    device: &str,
    device_path: &str,
    local: &str,
    policy: Policy,
) -> Result<u64, String> {
    let (device, device_path, local) = (
        device.to_string(),
        device_path.to_string(),
        local.to_string(),
    );
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let (overwrite, verify) = policy;
        match proxy
            .enqueue_pull_with_options(&device, &device_path, &local, &overwrite, verify)
            .await
        {
            Ok(id) => Ok(id),
            Err(err) if is_unknown_method(&err) => {
                Ok(proxy.enqueue_pull(&device, &device_path, &local).await?)
            }
            Err(err) => Err(err.into()),
        }
    })
    .await
}

/// Recursively queue a whole directory for upload.
pub async fn enqueue_tree_push(
    device: &str,
    local_dir: &str,
    device_dir: &str,
    policy: Policy,
) -> Result<crate::protocol::TreeEnqueueResult, String> {
    let (device, local_dir, device_dir) = (
        device.to_string(),
        local_dir.to_string(),
        device_dir.to_string(),
    );
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let (overwrite, verify) = policy;
        let json = proxy
            .enqueue_tree_push(&device, &local_dir, &device_dir, &overwrite, verify)
            .await?;
        Ok(serde_json::from_str(&json)?)
    })
    .await
}

/// Recursively queue a whole directory for download.
pub async fn enqueue_tree_pull(
    device: &str,
    device_dir: &str,
    local_dir: &str,
    policy: Policy,
) -> Result<crate::protocol::TreeEnqueueResult, String> {
    let (device, device_dir, local_dir) = (
        device.to_string(),
        device_dir.to_string(),
        local_dir.to_string(),
    );
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let (overwrite, verify) = policy;
        let json = proxy
            .enqueue_tree_pull(&device, &device_dir, &local_dir, &overwrite, verify)
            .await?;
        Ok(serde_json::from_str(&json)?)
    })
    .await
}

/// Copy a directory tree on the device itself, without going through the host.
pub async fn copy_tree(
    device: &str,
    src_dir: &str,
    dst_dir: &str,
) -> Result<crate::protocol::TreeEnqueueResult, String> {
    let (device, src_dir, dst_dir) = (device.to_string(), src_dir.to_string(), dst_dir.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        let json = proxy.copy_tree(&device, &src_dir, &dst_dir).await?;
        Ok(serde_json::from_str(&json)?)
    })
    .await
}

/// Current queue contents.
pub async fn list_jobs() -> Result<Vec<JobInfo>, String> {
    dispatch(|| async {
        let proxy = get_manager().await?;
        let json = proxy.list_jobs().await?;
        let jobs: Vec<JobDto> = serde_json::from_str(&json)?;
        Ok(jobs.into_iter().map(JobInfo::from).collect())
    })
    .await
}

macro_rules! job_control {
    ($($name:ident => $method:ident),* $(,)?) => {
        $(
            /// Forward a control call to the daemon and report whether it took.
            pub async fn $name(id: u64) -> Result<bool, String> {
                dispatch(move || async move {
                    let proxy = get_manager().await?;
                    Ok(proxy.$method(id).await?)
                })
                .await
            }
        )*
    };
}

job_control! {
    pause_job => pause_job,
    resume_job => resume_job,
    cancel_job => cancel_job,
}

/// Re-queue every job that ended in `Failed`.
pub async fn retry_failed() -> Result<u64, String> {
    dispatch(move || async move {
        let proxy = get_manager().await?;
        Ok(proxy.retry_failed().await?)
    })
    .await
}

/// True when the daemon does not implement a method we would rather use.
fn is_unknown_method(err: &zbus::Error) -> bool {
    matches!(
        err,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
    )
}

// ── Mutations ────────────────────────────────────────────────────────────────

/// Create one directory on a device.
pub async fn mkdir(device: &str, path: &str) -> Result<(), String> {
    let (device, path) = (device.to_string(), path.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        proxy.mkdir(&device, &path).await?;
        Ok(())
    })
    .await
}

/// Rename or move a path on a device.
pub async fn rename(device: &str, src: &str, dst: &str) -> Result<(), String> {
    let (device, src, dst) = (device.to_string(), src.to_string(), dst.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        proxy.rename(&device, &src, &dst).await?;
        Ok(())
    })
    .await
}

/// Copy a single file on a device.
pub async fn copy_file(device: &str, src: &str, dst: &str) -> Result<(), String> {
    let (device, src, dst) = (device.to_string(), src.to_string(), dst.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        proxy.copy_file(&device, &src, &dst).await?;
        Ok(())
    })
    .await
}

/// Delete a path on a device. This is permanent; there is no device-side trash.
pub async fn delete(device: &str, path: &str) -> Result<(), String> {
    let (device, path) = (device.to_string(), path.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        proxy.delete(&device, &path).await?;
        Ok(())
    })
    .await
}

/// Install an APK from a local path onto a device.
pub async fn install_apk(device: &str, path: &str) -> Result<String, String> {
    let (device, path) = (device.to_string(), path.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        Ok(proxy.install_apk(&device, &path).await?)
    })
    .await
}

// ── Wireless pairing ─────────────────────────────────────────────────────────

/// Step 1 of wireless pairing: hand the daemon an address and a pairing code.
pub async fn pair_wireless(address: &str, code: &str) -> Result<String, String> {
    let (address, code) = (address.to_string(), code.to_string());
    dispatch(move || async move {
        let proxy = get_manager().await?;
        Ok(proxy.pair_wireless(&address, &code).await?)
    })
    .await
}

/// Step 2 of wireless pairing: connect to an already-paired address.
pub async fn connect_wireless(address: &str) -> Result<String, String> {
    let address = address.to_string();
    dispatch(move || async move {
        let proxy = get_manager().await?;
        Ok(proxy.connect_wireless(&address).await?)
    })
    .await
}

// ── Diagnostics ──────────────────────────────────────────────────────────────

/// The daemon's health report.
pub async fn diagnostics() -> Result<crate::protocol::DiagnosticReportDto, String> {
    dispatch(|| async {
        let proxy = get_manager().await?;
        let json = proxy.diagnostics().await?;
        Ok(serde_json::from_str(&json)?)
    })
    .await
}

// ── Policy plumbing ──────────────────────────────────────────────────────────

/// Read the session-wide transfer policy.
pub fn transfer_policy(cx: &gpui::App) -> (String, bool) {
    let p = cx.global::<TransferPolicy>();
    (p.overwrite.clone(), p.verify)
}

/// Write the session-wide transfer policy.
pub fn set_transfer_policy(cx: &mut gpui::App, overwrite: &str, verify: bool) {
    cx.update_global::<TransferPolicy, _>(|p, _cx| {
        p.overwrite = overwrite.to_string();
        p.verify = verify;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "adbshare-daemon-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("temp dir");
        base
    }

    #[test]
    fn path_scan_finds_a_real_daemon_and_skips_missing_ones() {
        let good = temp_dir("good");
        std::fs::write(good.join("adb-daemon"), b"").expect("write fake daemon");
        let empty = temp_dir("empty");

        let paths = std::env::join_paths([&empty, &good]).unwrap();
        assert_eq!(
            daemon_on_path(Some(&paths)),
            Some(good.join("adb-daemon")),
            "the scan should skip directories with no daemon"
        );

        let only_empty = std::env::join_paths([&empty]).unwrap();
        assert_eq!(daemon_on_path(Some(&only_empty)), None);
        assert_eq!(daemon_on_path(None), None, "an unset PATH is not an error");

        std::fs::remove_dir_all(&good).ok();
        std::fs::remove_dir_all(&empty).ok();
    }

    #[test]
    fn common_dbus_failures_are_translated_into_advice() {
        // A name that cannot be activated is the overwhelmingly common case:
        // the daemon is not running and could not be started.
        for raw in [
            "org.freedesktop.DBus.Error.ServiceUnknown: The name org.adbshare.Manager was not provided by any .service files",
            "org.freedesktop.DBus.Error.NameHasNoOwner",
        ] {
            let advice = explain(raw);
            assert!(
                advice.contains("adb-daemon"),
                "{raw:?} should point at the daemon, got {advice:?}"
            );
            assert!(
                !advice.contains("org.freedesktop"),
                "the D-Bus error name should not leak to the user: {advice:?}"
            );
        }
        assert!(explain("AccessDenied").contains("desktop session"));
        assert!(explain("request timed out").contains("log"));
    }

    #[test]
    fn an_unrecognised_error_is_passed_through_verbatim() {
        // A specific error is usually the useful one, and the sidebar shows it.
        let raw = "device 'emulator-5554' not found";
        assert_eq!(explain(raw), raw);
    }

    #[tokio::test]
    async fn calls_report_a_missing_runtime_instead_of_panicking() {
        // Tests never call `start`, so every entry point must surface the
        // missing runtime as an error rather than unwinding.
        let err = list_devices().await.expect_err("runtime is absent");
        assert!(err.contains("D-Bus runtime"), "{err}");
    }
}
