use std::path::PathBuf;
use std::sync::Arc;

use adb_device::DeviceId;
use adb_proxy::ProxyClient;

use crate::filesystem::{Adbfs, FsError};

pub fn run(device: DeviceId, client: ProxyClient, mountpoint: PathBuf) -> Result<(), FsError> {
    use fuser::{MountOption, Session};

    let options = vec![
        MountOption::FSName(format!("adbshare:{}", device)),
        MountOption::Subtype("adbshare".to_string()),
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
    ];

    // Build a per-mount tokio runtime for the SyncProxy. The proxy client
    // uses tokio I/O and the SyncProxy thread will drive it.
    let rt = Arc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| FsError::Other(e.to_string()))?
    );
    let rt_thread = std::thread::Builder::new()
        .name(format!("adbfs-rt-{}", device))
        .spawn({
            let rt = rt.clone();
            move || {
                rt.block_on(async {
                    std::future::pending::<()>().await;
                });
            }
        })
        .map_err(|e| FsError::Other(e.to_string()))?;
    // Keep the runtime alive alongside the mount.
    std::mem::forget(rt_thread);
    let _rt = rt;

    let fs = Adbfs::new(client, _rt.handle().clone());
    let session = Session::new(fs, &mountpoint, &options).map_err(FsError::Fuse)?;
    eprintln!("adbfs: BackgroundSession starting on {:?}", mountpoint);
    let _bg = fuser::BackgroundSession::new(session).map_err(FsError::Fuse)?;
    eprintln!("adbfs: waiting for unmount");
    // Keep the Session alive. BackgroundSession holds the mount internally;
    // dropping _bg would unmount. We park forever.
    std::thread::park();
    Ok(())
}
