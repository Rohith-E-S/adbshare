use std::fs;
use std::future::Future;
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adb_proxy::{OpenFlags, ProxyClient, ProxyError, Stat, Status};
use tokio::time::{sleep, timeout};

const TIMEOUT: Duration = Duration::from_secs(5);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "adbshare-filesystem-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().unwrap().to_owned()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Helper(Child);

static START_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Helper {
    // The guard deliberately spans the readiness await below: the port has to
    // stay claimed from the bind until the helper is answering on it.
    #[allow(clippy::await_holding_lock)]
    async fn start() -> (Self, ProxyClient) {
        // Serialized: picking an ephemeral port then handing it to the
        // helper is racy when tests start helpers in parallel — a sibling
        // test's helper can bind the just-released port first, and its
        // teardown then closes our connection mid-test.
        // Take the lock even if a previous helper poisoned it on its way out,
        // so one failure does not cascade into eight PoisonErrors.
        let _startup = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let mut helper = Self(
            Command::new(env!("CARGO_BIN_EXE_adbshare-proxy"))
                .arg(addr.port().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let client = timeout(TIMEOUT, async {
            loop {
                assert!(
                    helper.0.try_wait().unwrap().is_none(),
                    "helper exited before readiness"
                );
                match ProxyClient::connect(addr.to_string(), 1).await {
                    Ok(client) => return client,
                    Err(_) => sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("helper readiness timed out");
        assert!(
            helper.0.try_wait().unwrap().is_none(),
            "helper exited during readiness"
        );
        assert_eq!(client.max_conns(), 1);
        (helper, client)
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn bounded<T>(operation: impl Future<Output = T>) -> T {
    timeout(TIMEOUT, operation)
        .await
        .expect("filesystem operation timed out")
}

fn assert_metadata(stat: Stat, metadata: fs::Metadata) {
    assert_eq!(stat.mode.0, metadata.mode());
    assert_eq!(stat.size, metadata.len());
    assert_eq!(stat.mtime, metadata.mtime());
    assert_eq!(stat.atime, metadata.atime());
    assert_eq!(stat.ctime, metadata.ctime());
    assert_eq!(u64::from(stat.nlink), metadata.nlink());
    assert_eq!(stat.uid, metadata.uid());
    assert_eq!(stat.gid, metadata.gid());
    assert_eq!(u64::from(stat.blksize), metadata.blksize());
    assert_eq!(stat.blocks, metadata.blocks());
}

/// A read must return exactly the bytes at the offset asked for, whatever was
/// in the read buffer before.
///
/// Reads are length-prefixed and the host asks for less than a chunk whenever
/// it feels like it, so a read that answered with a stale buffer or with zeros
/// past the end of the file would be invisible on a large chunk and obvious on
/// a small one. Reading large-then-small is the case that catches it.
#[tokio::test(flavor = "current_thread")]
async fn a_small_read_after_a_large_one_returns_only_its_own_bytes() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let path = dir.path("chunked.bin");

    // A megabyte, so the first read is a large chunk like a real download.
    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNC;
    let file = bounded(client.open(&path, flags, 0o600)).await.unwrap();
    bounded(file.write_at(0, &payload)).await.unwrap();

    // A full-size read, then progressively smaller ones at other offsets.
    let whole = bounded(file.read_at(0, payload.len() as u32))
        .await
        .unwrap();
    assert_eq!(whole.len(), payload.len(), "the first read is chunk-sized");
    assert_eq!(whole[..], payload[..]);

    for (offset, len) in [(0u64, 1usize), (1, 3), (4096, 7), (999_999, 2), (0, 0)] {
        let got = bounded(file.read_at(offset, len as u32)).await.unwrap();
        assert_eq!(
            got.len(),
            len,
            "read at {offset} of {len} returned {} bytes",
            got.len()
        );
        assert_eq!(
            got[..],
            payload[offset as usize..offset as usize + len][..],
            "read at {offset} of {len} returned the wrong bytes"
        );
    }

    // And a read that runs off the end returns only what exists.
    let tail = bounded(file.read_at(payload.len() as u64 - 4, 64))
        .await
        .unwrap();
    assert_eq!(
        tail.len(),
        4,
        "a read past the end is truncated, not padded"
    );
    assert_eq!(tail[..], payload[payload.len() - 4..]);

    bounded(file.close()).await.unwrap();
    assert_eq!(fs::read(&path).unwrap(), payload);
}

#[tokio::test(flavor = "current_thread")]
async fn mkdir_exclusive_open_offset_io_and_close() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let folder = dir.path("created directory");
    bounded(client.mkdir(&folder, 0o700)).await.unwrap();
    assert!(fs::metadata(&folder).unwrap().is_dir());
    let path = dir.path("created directory/file");
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCL;
    let file = bounded(client.open(&path, flags, 0o600)).await.unwrap();
    bounded(file.write_at(0, b"abcdefghij")).await.unwrap();
    bounded(file.write_at(3, b"XYZ")).await.unwrap();
    bounded(file.write_at(12, b"end")).await.unwrap();
    assert_eq!(&bounded(file.read_at(2, 5)).await.unwrap()[..], b"cXYZg");
    assert_eq!(
        &bounded(file.read_at(9, 20)).await.unwrap()[..],
        b"j\0\0end"
    );
    assert!(bounded(file.read_at(15, 1)).await.unwrap().is_empty());
    assert!(bounded(file.read_at(100, 16)).await.unwrap().is_empty());
    bounded(file.close()).await.unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"abcXYZghij\0\0end");
    assert!(matches!(
        bounded(client.open(&path, flags, 0o600)).await,
        Err(ProxyError::Status(_, _))
    ));
    assert_metadata(
        bounded(client.stat(&path)).await.unwrap(),
        fs::metadata(&path).unwrap(),
    );
    let file = bounded(client.open(&path, OpenFlags::READ, 0))
        .await
        .unwrap();
    assert_eq!(
        &bounded(file.read_at(0, 100)).await.unwrap()[..],
        b"abcXYZghij\0\0end"
    );
    bounded(file.close()).await.unwrap();
    assert_metadata(
        bounded(client.stat(&folder)).await.unwrap(),
        fs::metadata(&folder).unwrap(),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn truncate_shrinks_clears_and_zero_extends() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let path = dir.path("truncate");
    fs::write(&path, b"abcdef").unwrap();
    for (size, expected) in [
        (3, b"abc".as_slice()),
        (7, b"abc\0\0\0\0"),
        (0, b""),
        (4, b"\0\0\0\0"),
    ] {
        bounded(client.truncate(&path, size)).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);
        let file = bounded(client.open(&path, OpenFlags::READ, 0))
            .await
            .unwrap();
        assert_eq!(&bounded(file.read_at(0, 32)).await.unwrap()[..], expected);
        assert!(bounded(file.read_at(size, 1)).await.unwrap().is_empty());
        bounded(file.close()).await.unwrap();
        assert_eq!(bounded(client.stat(&path)).await.unwrap().size, size);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn read_link_returns_exact_relative_target() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let target = "target with spaces";
    fs::write(dir.path(target), b"target").unwrap();
    let link = dir.path("link");
    symlink(target, &link).unwrap();
    assert_eq!(bounded(client.read_link(&link)).await.unwrap(), target);
    assert_metadata(
        bounded(client.lstat(&link)).await.unwrap(),
        fs::symlink_metadata(&link).unwrap(),
    );
    assert_metadata(
        bounded(client.stat(&link)).await.unwrap(),
        fs::metadata(&link).unwrap(),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn real_path_resolves_fixture_symlink_exactly() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let target = dir.path("target with spaces");
    fs::write(&target, b"target").unwrap();
    let link = dir.path("link");
    symlink("target with spaces", &link).unwrap();
    let expected = fs::canonicalize(&target).unwrap();
    assert_eq!(
        bounded(client.real_path(&link)).await.unwrap(),
        expected.to_str().unwrap()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn utime_is_observed_by_stat() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let path = dir.path("timestamps");
    fs::write(&path, b"timestamp fixture").unwrap();
    let atime = 1_600_000_123;
    let mtime = 1_650_000_456;
    bounded(client.utime(&path, atime, mtime)).await.unwrap();
    let stat = bounded(client.stat(&path)).await.unwrap();
    assert_eq!(stat.atime, atime);
    assert_eq!(stat.mtime, mtime);
    assert_metadata(stat, fs::metadata(&path).unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn listdir_reports_each_entry_with_metadata() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    fs::write(dir.path("file with spaces"), b"directory entry").unwrap();
    fs::write(dir.path("empty"), b"").unwrap();
    fs::create_dir(dir.path("nested")).unwrap();
    symlink("file with spaces", dir.path("link")).unwrap();
    let mut entries = bounded(client.listdir(dir.0.to_str().unwrap()))
        .await
        .unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["empty", "file with spaces", "link", "nested"]
    );
    for entry in entries {
        assert_metadata(
            entry.stat,
            fs::symlink_metadata(dir.path(&entry.name)).unwrap(),
        );
    }
    assert!(
        bounded(client.listdir(&dir.path("nested")))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn disk_usage_has_sensible_byte_counts() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let usage = bounded(client.disk_usage(dir.0.to_str().unwrap()))
        .await
        .unwrap();
    assert!(usage.total_bytes > 0);
    assert!(usage.avail_bytes <= usage.total_bytes);
}

#[tokio::test(flavor = "current_thread")]
async fn copy_preserves_source_and_refuses_existing_destination() {
    let dir = TestDir::new();
    let (_helper, client) = Helper::start().await;
    let src = dir.path("source");
    let dst = dir.path("destination");
    let existing = dir.path("existing");
    let data: Vec<u8> = (0..131_089).map(|i| (i % 251) as u8).collect();
    fs::write(&src, &data).unwrap();
    fs::write(&existing, b"keep destination").unwrap();
    let before = fs::metadata(&src).unwrap();
    bounded(client.copy_file(&src, &dst)).await.unwrap();
    assert_eq!(fs::read(&src).unwrap(), data);
    assert_eq!(fs::read(&dst).unwrap(), data);
    assert_ne!(before.ino(), fs::metadata(&dst).unwrap().ino());
    assert!(matches!(
        bounded(client.copy_file(&src, &existing)).await,
        Err(ProxyError::Status(Status::Exists, _))
    ));
    assert_eq!(fs::read(&existing).unwrap(), b"keep destination");
    assert_eq!(fs::read(&src).unwrap(), data);
    let after = fs::metadata(&src).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.len(), after.len());
    assert_eq!(before.mode(), after.mode());
    assert_eq!(before.mtime(), after.mtime());
    assert_eq!(before.mtime_nsec(), after.mtime_nsec());
    assert_metadata(bounded(client.stat(&src)).await.unwrap(), after);
}
