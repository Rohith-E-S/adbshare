use std::path::PathBuf;

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

    // The SyncProxy thread builds and owns its own tokio runtime; nothing
    // async is needed on this thread.
    let fs = Adbfs::new(client);
    let session = Session::new(fs, &mountpoint, &options).map_err(FsError::Fuse)?;
    eprintln!("adbfs: BackgroundSession starting on {:?}", mountpoint);
    let _bg = fuser::BackgroundSession::new(session).map_err(FsError::Fuse)?;
    eprintln!("adbfs: waiting for unmount");
    // Keep the Session alive. BackgroundSession holds the mount
    // internally; dropping _bg would unmount. `park` can return
    // spuriously, so re-park in a loop — we never expect a real wake-up
    // here. The mount stays up until the process exits or this thread is
    // killed; unmounting is done externally (fusermount3 / the daemon
    // tearing the thread down). Signalling this thread to return cleanly
    // on external unmount would need a shared handle across crates and is
    // left as a follow-up.
    loop {
        std::thread::park();
    }
}
