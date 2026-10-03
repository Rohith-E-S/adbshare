//! FUSE filesystem implementation backed by an `adb_proxy::ProxyClient`.
//!
//! The filesystem is rooted at the device's `/`. We track ino -> path so
//! the kernel can refer back to entries after `lookup` returns. Proxy
//! calls go through a dedicated background thread that owns the proxy's
//! tokio runtime; the FUSE callbacks (which run on fuser worker threads
//! — NOT tokio workers) issue blocking calls through this wrapper.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fuser::{
    FUSE_ROOT_ID, FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};
use parking_lot::Mutex;
use thiserror::Error;

use adb_proxy::{DirEntry, FileMode, OpenFlags, ProxyClient, ProxyError, ProxyFile, Stat, Status};

use crate::cache::{DirCache, StatCache};

const TTL: Duration = Duration::from_secs(2);

/// How long a directory listing is reused across `readdir` calls.
///
/// Shorter than a stat's worth of staleness would be surprising to a user
/// watching a file appear on the phone, and a listing is only consulted for
/// entries that already exist.
const DIR_TTL: Duration = Duration::from_secs(2);
/// How many directories to remember. A file manager or an open dialog walks one
/// tree at a time, so a handful covers the working set without letting a long
/// session accumulate listings.
const DIR_CACHE_CAPACITY: usize = 8;
const BLOCK_SIZE: u32 = 4096;
const ADB_UID: u32 = 2000;
const ADB_GID: u32 = 2000;
/// How long to wait between proxy reconnect attempts.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum FsError {
    #[error("fuse: {0}")]
    Fuse(std::io::Error),
    #[error("proxy: {0}")]
    Proxy(#[from] ProxyError),
    #[error("{0}")]
    Other(String),
}

pub struct Adbfs {
    proxy: SyncProxy,
    cache: StatCache,
    /// Recent directory listings, so a continued `readdir` does not re-list
    /// the whole directory over ADB.
    dir_cache: DirCache,
    ino_to_path: Mutex<HashMap<u64, PathBuf>>,
    path_to_ino: Mutex<HashMap<PathBuf, u64>>,
    next_ino: AtomicU64,
    open_files: Mutex<HashMap<u64, OpenFile>>,
    next_fh: AtomicU64,
}

struct OpenFile {
    proxy: ProxyFile,
}

impl std::fmt::Debug for Adbfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adbfs").finish()
    }
}

/// Convert a path into the UTF-8 string the proxy protocol requires.
///
/// Non-UTF-8 filenames are legal on Linux; the proxy protocol carries
/// JSON strings and cannot represent them, so this returns `None` and
/// callers reply with an errno instead of panicking the FUSE thread.
fn path_to_string(p: &Path) -> Option<String> {
    p.to_str().map(|s| s.to_string())
}

/// Synchronous proxy wrapper. Spawns a dedicated thread that owns a
/// tokio current-thread runtime and drives the proxy client on it. FUSE
/// callback threads (which are NOT tokio workers) dispatch requests to
/// that thread over an mpsc channel and block on the reply channel.
#[derive(Clone)]
struct SyncProxy {
    tx: tokio::sync::mpsc::UnboundedSender<ProxyRequest>,
}

type Reply<T> = std::sync::mpsc::Sender<std::result::Result<T, ProxyError>>;

enum ProxyRequest {
    Stat(String, Reply<Stat>),
    ListDir(String, Reply<Vec<DirEntry>>),
    Open(String, OpenFlags, u32, Reply<ProxyFile>),
    ReadAt(ProxyFile, u64, u32, Reply<bytes::Bytes>),
    WriteAt(ProxyFile, u64, Vec<u8>, Reply<()>),
    Close(ProxyFile, Reply<()>),
    Mkdir(String, u32, Reply<()>),
    Unlink(String, Reply<()>),
    Rmdir(String, Reply<()>),
    Rename(String, String, Reply<()>),
    Truncate(String, u64, Reply<()>),
}

impl SyncProxy {
    fn start(client: ProxyClient) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProxyRequest>();
        let addr = client.addr().to_string();
        let max_conns = client.max_conns();
        // The ProxyClient we received was connected on a different
        // tokio runtime (the daemon's). Its TcpStreams are bound to
        // that runtime's I/O reactor. If we use them from this proxy
        // thread's runtime, I/O events go to the wrong thread and
        // requests never complete. So we drop that client and reconnect
        // on this thread's runtime, which is the right place.
        drop(client);

        std::thread::Builder::new()
            .name("adbfs-proxy".into())
            .spawn(move || {
                // The proxy thread owns its own single-threaded tokio
                // runtime. The TcpStreams we open here are registered
                // with THIS runtime's I/O reactor, which is being driven
                // by THIS thread. So events are delivered correctly.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build proxy runtime");
                let _enter = rt.enter();
                // Connect with retries: a temporarily-offline device
                // (unplugged USB, restarting adbd) should recover instead
                // of leaving the mount permanently returning EIO.
                let client = loop {
                    match rt.block_on(ProxyClient::connect(&addr, max_conns)) {
                        Ok(c) => break std::sync::Arc::new(c),
                        Err(e) => {
                            eprintln!(
                                "adbfs: proxy connect to {addr} failed: {e}; retrying in {:?}",
                                CONNECT_RETRY_INTERVAL
                            );
                            std::thread::sleep(CONNECT_RETRY_INTERVAL);
                        }
                    }
                };

                rt.block_on(async move {
                    while let Some(req) = rx.recv().await {
                        match req {
                            ProxyRequest::Stat(path, s) => {
                                let r = client.stat(&path).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::ListDir(path, s) => {
                                let r = client.listdir(&path).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Open(path, flags, mode, s) => {
                                let r = client.open(&path, flags, mode).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::ReadAt(file, off, len, s) => {
                                let r = file.read_at(off, len).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::WriteAt(file, off, data, s) => {
                                let r = file.write_at(off, &data).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Close(file, s) => {
                                let r = file.close().await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Mkdir(path, mode, s) => {
                                let r = client.mkdir(&path, mode).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Unlink(path, s) => {
                                let r = client.unlink(&path).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Rmdir(path, s) => {
                                let r = client.rmdir(&path).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Rename(src, dst, s) => {
                                let r = client.rename(&src, &dst).await;
                                let _ = s.send(r);
                            }
                            ProxyRequest::Truncate(path, size, s) => {
                                let r = client.truncate(&path, size).await;
                                let _ = s.send(r);
                            }
                        }
                    }
                });
            })
            .expect("spawn proxy thread");
        Self { tx }
    }

    /// Issue a request and synchronously wait for the result.
    fn call<F, R>(&self, build: F) -> std::result::Result<R, ProxyError>
    where
        F: FnOnce(Reply<R>) -> ProxyRequest,
        R: 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let req = build(tx);
        if self.tx.send(req).is_err() {
            return Err(ProxyError::Closed);
        }
        rx.recv().map_err(|_| ProxyError::Closed)?
    }

    fn stat(&self, path: &str) -> std::result::Result<Stat, ProxyError> {
        self.call(|tx| ProxyRequest::Stat(path.to_string(), tx))
    }
    fn listdir(&self, path: &str) -> std::result::Result<Vec<DirEntry>, ProxyError> {
        self.call(|tx| ProxyRequest::ListDir(path.to_string(), tx))
    }
    fn open(
        &self,
        path: &str,
        flags: OpenFlags,
        mode: u32,
    ) -> std::result::Result<ProxyFile, ProxyError> {
        self.call(|tx| ProxyRequest::Open(path.to_string(), flags, mode, tx))
    }
    fn read_at(
        &self,
        file: ProxyFile,
        off: u64,
        len: u32,
    ) -> std::result::Result<bytes::Bytes, ProxyError> {
        self.call(|tx| ProxyRequest::ReadAt(file, off, len, tx))
    }
    fn write_at(
        &self,
        file: ProxyFile,
        off: u64,
        data: Vec<u8>,
    ) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::WriteAt(file, off, data, tx))
    }
    fn close(&self, file: ProxyFile) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::Close(file, tx))
    }
    fn mkdir(&self, path: &str, mode: u32) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::Mkdir(path.to_string(), mode, tx))
    }
    fn unlink(&self, path: &str) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::Unlink(path.to_string(), tx))
    }
    fn rmdir(&self, path: &str) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::Rmdir(path.to_string(), tx))
    }
    fn rename(&self, src: &str, dst: &str) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::Rename(src.to_string(), dst.to_string(), tx))
    }
    fn truncate(&self, path: &str, size: u64) -> std::result::Result<(), ProxyError> {
        self.call(|tx| ProxyRequest::Truncate(path.to_string(), size, tx))
    }
}

impl Adbfs {
    /// Create the FS. `client` is moved into a dedicated background
    /// thread that owns a tokio runtime; the FUSE callbacks stay
    /// synchronous and issue blocking requests through the SyncProxy.
    pub fn new(client: ProxyClient) -> Self {
        Self::with_proxy(SyncProxy::start(client))
    }

    fn with_proxy(proxy: SyncProxy) -> Self {
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();
        ino_to_path.insert(FUSE_ROOT_ID, PathBuf::from("/"));
        path_to_ino.insert(PathBuf::from("/"), FUSE_ROOT_ID);
        Self {
            proxy,
            cache: StatCache::new(TTL),
            dir_cache: DirCache::new(DIR_TTL, DIR_CACHE_CAPACITY),
            ino_to_path: Mutex::new(ino_to_path),
            path_to_ino: Mutex::new(path_to_ino),
            next_ino: AtomicU64::new(100),
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    /// Drop the cached listing of whatever directory contains `path`.
    ///
    /// A mutation changes the *parent's* entries, not the mutated path's, so
    /// every create, delete, rename and truncate has to invalidate the parent.
    fn invalidate_parent_listing(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.dir_cache.invalidate(parent);
            // The parent gained or lost an entry, so its own size, mtime and
            // link count are stale too.
            self.cache.invalidate(parent);
        }
    }

    /// Drop the caches a completed rename left lying.
    ///
    /// Both sides lose an entry from the directory that holds them, whether or
    /// not what moved was a directory, so both parents' listings go. The stats
    /// for the two paths are stale either way; if a directory moved, everything
    /// under it moved with it.
    fn invalidate_after_rename(&self, src: &Path, dst: &Path, is_dir: bool) {
        self.invalidate_parent_listing(src);
        self.invalidate_parent_listing(dst);
        if is_dir {
            self.cache.invalidate_prefix(src);
            self.cache.invalidate_prefix(dst);
            self.dir_cache.invalidate_prefix(src);
            self.dir_cache.invalidate_prefix(dst);
        } else {
            self.cache.invalidate(src);
            self.cache.invalidate(dst);
        }
    }

    fn ino_for(&self, path: PathBuf) -> u64 {
        let mut p2i = self.path_to_ino.lock();
        if let Some(&ino) = p2i.get(&path) {
            return ino;
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        p2i.insert(path.clone(), ino);
        self.ino_to_path.lock().insert(ino, path);
        ino
    }

    fn attr_from_stat(&self, ino: u64, stat: Stat) -> FileAttr {
        let kind = Self::kind_from_mode(&stat.mode);
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.mtime.max(0) as u64);
        let atime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.atime.max(0) as u64);
        let ctime = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.ctime.max(0) as u64);
        FileAttr {
            ino,
            size: stat.size,
            blocks: stat.blocks,
            atime,
            mtime,
            ctime,
            crtime: ctime,
            kind,
            perm: stat.mode.permissions() as u16,
            nlink: stat.nlink,
            uid: ADB_UID,
            gid: ADB_GID,
            rdev: 0,
            blksize: stat.blksize.max(BLOCK_SIZE),
            flags: 0,
        }
    }

    fn kind_from_mode(mode: &FileMode) -> FileType {
        if mode.is_dir() {
            FileType::Directory
        } else if mode.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        }
    }

    fn resolve_child(&self, parent: u64, name: &OsStr) -> Option<PathBuf> {
        let parent_path = self.ino_to_path.lock().get(&parent).cloned()?;
        let mut p = parent_path;
        p.push(name);
        Some(p)
    }

    /// Give one entry of a listing its inode and return it.
    ///
    /// `fresh` says whether the listing was just fetched over the proxy. Only
    /// then are its stats worth caching: the snapshot a cache hit serves is
    /// already up to a TTL old, and re-stamping it would keep answering with
    /// it for another TTL on top of that.
    fn adopt_listing_entry(&self, parent: &Path, entry: &DirEntry, fresh: bool) -> u64 {
        let mut child_path = parent.to_path_buf();
        child_path.push(OsStr::from_bytes(entry.name.as_bytes()));
        let ino = self.ino_for(child_path.clone());
        if fresh {
            self.cache.put(child_path, entry.stat);
        }
        ino
    }

    fn proxy_to_errno(e: ProxyError) -> i32 {
        match e {
            ProxyError::Status(Status::NotFound, _) => libc::ENOENT,
            ProxyError::Status(Status::PermissionDenied, _) => libc::EACCES,
            ProxyError::Status(Status::IsDir, _) => libc::EISDIR,
            ProxyError::Status(Status::NotDir, _) => libc::ENOTDIR,
            ProxyError::Status(Status::Exists, _) => libc::EEXIST,
            ProxyError::Status(Status::NotEmpty, _) => libc::ENOTEMPTY,
            ProxyError::Status(Status::NoSpace, _) => libc::ENOSPC,
            ProxyError::Status(Status::NameTooLong, _) => libc::ENAMETOOLONG,
            ProxyError::Status(Status::InvalidArg, _) => libc::EINVAL,
            _ => libc::EIO,
        }
    }
}

impl Filesystem for Adbfs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let path = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let res = self.proxy.stat(&path_str);
        match res {
            Ok(stat) => {
                self.cache.put(path.clone(), stat);
                let ino = self.ino_for(path);
                let attr = self.attr_from_stat(ino, stat);
                reply.entry(&TTL, &attr, 0);
            }
            Err(ProxyError::Status(Status::NotFound, _)) => reply.error(libc::ENOENT),
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let path = self.ino_to_path.lock().get(&ino).cloned();
        let path = match path {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        if let Some(stat) = self.cache.get(&path) {
            let attr = self.attr_from_stat(ino, stat);
            reply.attr(&TTL, &attr);
            return;
        }
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let res = self.proxy.stat(&path_str);
        match res {
            Ok(stat) => {
                self.cache.put(path, stat);
                let attr = self.attr_from_stat(ino, stat);
                reply.attr(&TTL, &attr);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = self.ino_to_path.lock().get(&ino).cloned();
        let path = match path {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        // FUSE walks a directory in as many `readdir` calls as the consumer
        // needs, each with a continuation offset. Listing the whole thing over
        // ADB every time made a large directory quadratic in round trips, so a
        // recent listing is reused.
        let (entries, fresh) = match self.dir_cache.get(&path) {
            Some(cached) => (cached, false),
            None => {
                let epoch = self.dir_cache.epoch();
                match self.proxy.listdir(&path_str) {
                    Ok(entries) => {
                        let entries = Arc::new(entries);
                        self.dir_cache
                            .put(path.clone(), Arc::clone(&entries), epoch);
                        (entries, true)
                    }
                    Err(e) => {
                        reply.error(Self::proxy_to_errno(e));
                        return;
                    }
                }
            }
        };
        let mut cur = offset.max(0) as usize;
        if cur == 0 {
            let _ = reply.add(ino, 1, FileType::Directory, ".");
            cur = 1;
        }
        // NOTE: `..` is advertised with FUSE_ROOT_ID, not the real parent
        // inode. The kernel resolves parents through its own dentry cache,
        // so this works in practice; reworking parent inode tracking is a
        // separate change.
        if cur == 1 {
            let _ = reply.add(FUSE_ROOT_ID, 2, FileType::Directory, "..");
            cur = 2;
        }
        for (n, entry) in entries.iter().enumerate().skip(cur.saturating_sub(2)) {
            let child_ino = self.adopt_listing_entry(&path, entry, fresh);
            let kind = Self::kind_from_mode(&entry.stat.mode);
            let _ = reply.add(child_ino, (n as i64) + 3, kind, &entry.name);
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let path = self.ino_to_path.lock().get(&ino).cloned();
        let path = match path {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let mut oflags = OpenFlags::READ;
        if flags & libc::O_WRONLY != 0 {
            oflags = OpenFlags::WRITE;
        }
        if flags & libc::O_RDWR != 0 {
            oflags = OpenFlags::READ | OpenFlags::WRITE;
        }
        if flags & libc::O_CREAT != 0 {
            oflags |= OpenFlags::CREATE;
        }
        if flags & libc::O_TRUNC != 0 {
            oflags |= OpenFlags::TRUNC;
        }
        if flags & libc::O_APPEND != 0 {
            oflags |= OpenFlags::APPEND;
        }
        let mode = 0o644;
        let res = self.proxy.open(&path_str, oflags, mode);
        match res {
            Ok(file) => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.open_files.lock().insert(fh, OpenFile { proxy: file });
                reply.opened(fh, 0);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let file = { self.open_files.lock().get(&fh).map(|f| f.proxy.clone()) };
        let file = match file {
            Some(f) => f,
            None => {
                reply.error(libc::EBADF);
                return;
            }
        };
        let res = self.proxy.read_at(file, offset as u64, size);
        match res {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let file = { self.open_files.lock().get(&fh).map(|f| f.proxy.clone()) };
        let file = match file {
            Some(f) => f,
            None => {
                reply.error(libc::EBADF);
                return;
            }
        };
        let res = self.proxy.write_at(file, offset as u64, data.to_vec());
        match res {
            Ok(()) => {
                if let Some(path) = self.ino_to_path.lock().get(&ino) {
                    self.cache.invalidate(path);
                    // A write changes the mtime the parent listing shows, so
                    // the cached listing is stale even though the set of
                    // entries is not.
                    self.invalidate_parent_listing(path);
                }
                reply.written(data.len() as u32);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(file) = self.open_files.lock().remove(&fh) {
            let _ = self.proxy.close(file.proxy);
        }
        if let Some(path) = self.ino_to_path.lock().get(&ino) {
            self.cache.invalidate(path);
        }
        reply.ok();
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let path = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let mut oflags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
        if flags & libc::O_TRUNC != 0 {
            oflags |= OpenFlags::TRUNC;
        }
        if flags & libc::O_EXCL != 0 {
            oflags |= OpenFlags::EXCL;
        }
        let res = self.proxy.open(&path_str, oflags, mode);
        match res {
            Ok(file) => {
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.open_files.lock().insert(fh, OpenFile { proxy: file });
                let ino = self.ino_for(path.clone());
                let stat = self.proxy.stat(&path_str).unwrap_or(Stat {
                    mode: FileMode::file(),
                    size: 0,
                    mtime: 0,
                    atime: 0,
                    ctime: 0,
                    uid: ADB_UID,
                    gid: ADB_GID,
                    nlink: 1,
                    blksize: 4096,
                    blocks: 0,
                });
                self.invalidate_parent_listing(&path);
                self.cache.put(path, stat);
                let attr = self.attr_from_stat(ino, stat);
                reply.created(&TTL, &attr, 0, fh, 0);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let path = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.mkdir(&path_str, mode) {
            Ok(()) => {
                let ino = self.ino_for(path.clone());
                let stat = self.proxy.stat(&path_str).unwrap_or(Stat {
                    mode: FileMode::dir(),
                    size: 4096,
                    mtime: 0,
                    atime: 0,
                    ctime: 0,
                    uid: ADB_UID,
                    gid: ADB_GID,
                    nlink: 2,
                    blksize: 4096,
                    blocks: 8,
                });
                self.invalidate_parent_listing(&path);
                self.cache.put(path, stat);
                let attr = self.attr_from_stat(ino, stat);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.unlink(&path_str) {
            Ok(()) => {
                self.invalidate_parent_listing(&path);
                self.cache.invalidate(&path);
                reply.ok();
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.rmdir(&path_str) {
            Ok(()) => {
                // The directory itself disappears, and so does its entry in
                // the parent.
                self.dir_cache.invalidate(&path);
                self.invalidate_parent_listing(&path);
                self.cache.invalidate(&path);
                reply.ok();
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let src = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let dst = match self.resolve_child(newparent, newname) {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(src_str) = path_to_string(&src) else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some(dst_str) = path_to_string(&dst) else {
            reply.error(libc::EINVAL);
            return;
        };
        // Was src a directory? Ask *before* the rename, while src still exists;
        // afterwards a fallback stat can only ever come back NotFound, and a
        // directory rename would then skip invalidating its own subtree.
        let is_dir = self
            .cache
            .get(&src)
            .map(|s| s.mode.is_dir())
            .unwrap_or_else(|| {
                self.proxy
                    .stat(&src_str)
                    .ok()
                    .map(|s| s.mode.is_dir())
                    .unwrap_or(false)
            });
        match self.proxy.rename(&src_str, &dst_str) {
            Ok(()) => {
                // Move the inode mappings from src to dst so existing inode
                // numbers (and therefore the kernel's cached nodeids) stay
                // valid across the rename. For a directory rename, rewrite
                // every descendant path too.
                {
                    let mut p2i = self.path_to_ino.lock();
                    let mut i2p = self.ino_to_path.lock();

                    // If the rename overwrote an existing destination, its
                    // old inode is gone; drop it so a later lookup at that
                    // path allocates a fresh inode.
                    if let Some(dst_ino) = p2i.remove(&dst) {
                        i2p.remove(&dst_ino);
                    }

                    let mut moved: Vec<(u64, PathBuf)> = Vec::new();
                    if let Some(ino) = p2i.remove(&src) {
                        moved.push((ino, dst.clone()));
                    }
                    if is_dir {
                        let stale: Vec<PathBuf> = p2i
                            .keys()
                            .filter(|p| p.starts_with(&src))
                            .cloned()
                            .collect();
                        for old in stale {
                            if let Some(ino) = p2i.remove(&old) {
                                let rel = old.strip_prefix(&src).unwrap_or_else(|_| Path::new(""));
                                moved.push((ino, dst.join(rel)));
                            }
                        }
                    }
                    for (ino, new_path) in moved {
                        i2p.insert(ino, new_path.clone());
                        p2i.insert(new_path, ino);
                    }
                }

                // Both parents' listings are stale too: one is where the
                // entry went from, the other where it arrived.
                self.invalidate_after_rename(&src, &dst, is_dir);
                reply.ok();
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let path = self.ino_to_path.lock().get(&ino).cloned();
        let path = match path {
            Some(p) => p,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        if let Some(s) = size {
            // Truncate is the one setattr operation the proxy protocol
            // supports; propagate failures instead of swallowing them.
            if let Err(e) = self.proxy.truncate(&path_str, s) {
                reply.error(Self::proxy_to_errno(e));
                return;
            }
            // The size shown in the parent listing just changed.
            self.invalidate_parent_listing(&path);
        }
        if mode.is_some() || uid.is_some() || gid.is_some() || atime.is_some() || mtime.is_some() {
            // The proxy protocol has no chmod/chown/utimens ops. Fail
            // loudly instead of silently accepting and losing the change;
            // adding protocol support is a follow-up.
            reply.error(libc::ENOSYS);
            return;
        }
        match self.proxy.stat(&path_str) {
            Ok(stat) => {
                self.cache.put(path, stat);
                let attr = self.attr_from_stat(ino, stat);
                reply.attr(&TTL, &attr);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyStatfs) {
        let _ = ino;
        // The proxy protocol has no statfs op, so report large practical
        // values the way network filesystems commonly do. Adding a real
        // statfs op to the protocol is a possible follow-up.
        const TOTAL_BLOCKS: u64 = 1 << 32; // 16 TiB at 4 KiB blocks
        const FREE_BLOCKS: u64 = 1 << 31; // 8 TiB
        reply.statfs(
            TOTAL_BLOCKS,
            FREE_BLOCKS,
            FREE_BLOCKS,
            1 << 20,
            1 << 20,
            BLOCK_SIZE,
            256,
            4096,
        );
    }
}

/// The callbacks cannot be driven without a real mount — fuser's `Reply`
/// types have no public constructors — so these cover the cache bookkeeping
/// they are built on.
#[cfg(test)]
mod tests {
    use super::*;

    fn stat_of(size: u64) -> Stat {
        Stat {
            mode: FileMode::file(),
            size,
            mtime: 0,
            atime: 0,
            ctime: 0,
            uid: ADB_UID,
            gid: ADB_GID,
            nlink: 1,
            blksize: 4096,
            blocks: 0,
        }
    }

    fn listing(names: &[&str]) -> Arc<Vec<DirEntry>> {
        Arc::new(
            names
                .iter()
                .map(|name| DirEntry {
                    name: (*name).to_string(),
                    stat: stat_of(0),
                })
                .collect(),
        )
    }

    /// An `Adbfs` with no proxy behind it: the bookkeeping under test never
    /// issues a request, and a real client would need a device to talk to.
    /// A closed receiver is what answers one.
    fn detached() -> Adbfs {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ProxyRequest>();
        drop(rx);
        Adbfs::with_proxy(SyncProxy { tx })
    }

    #[test]
    fn renaming_a_file_drops_both_parents_listings() {
        let fs = detached();
        fs.dir_cache.put(
            PathBuf::from("/sdcard"),
            listing(&["old.bin"]),
            fs.dir_cache.epoch(),
        );
        fs.dir_cache.put(
            PathBuf::from("/sdcard/DCIM"),
            listing(&["keep"]),
            fs.dir_cache.epoch(),
        );
        fs.dir_cache.put(
            PathBuf::from("/data"),
            listing(&["other"]),
            fs.dir_cache.epoch(),
        );

        fs.invalidate_after_rename(
            Path::new("/sdcard/old.bin"),
            Path::new("/sdcard/new.bin"),
            false,
        );

        // readdir on the parent has to go back to the device: the entry it
        // would serve is gone, and one it never saw is missing.
        assert!(fs.dir_cache.get(Path::new("/sdcard")).is_none());
        assert!(
            fs.dir_cache.get(Path::new("/sdcard/DCIM")).is_some(),
            "a directory that was only walked through is untouched"
        );
        assert!(
            fs.dir_cache.get(Path::new("/data")).is_some(),
            "a directory the rename did not touch is untouched"
        );
    }

    #[test]
    fn renaming_a_file_into_another_directory_drops_both_parents() {
        let fs = detached();
        fs.dir_cache.put(
            PathBuf::from("/sdcard"),
            listing(&["a.bin"]),
            fs.dir_cache.epoch(),
        );
        fs.dir_cache.put(
            PathBuf::from("/sdcard/Movies"),
            listing(&[]),
            fs.dir_cache.epoch(),
        );

        fs.invalidate_after_rename(
            Path::new("/sdcard/a.bin"),
            Path::new("/sdcard/Movies/a.bin"),
            false,
        );

        assert!(fs.dir_cache.get(Path::new("/sdcard")).is_none());
        assert!(fs.dir_cache.get(Path::new("/sdcard/Movies")).is_none());
    }

    #[test]
    fn renaming_a_directory_drops_both_subtrees() {
        let fs = detached();
        fs.dir_cache.put(
            PathBuf::from("/sdcard/DCIM"),
            listing(&["old"]),
            fs.dir_cache.epoch(),
        );
        fs.dir_cache.put(
            PathBuf::from("/sdcard/DCIM/2024"),
            listing(&["kept"]),
            fs.dir_cache.epoch(),
        );
        fs.dir_cache.put(
            PathBuf::from("/sdcard/Movies"),
            listing(&[]),
            fs.dir_cache.epoch(),
        );

        fs.invalidate_after_rename(
            Path::new("/sdcard/DCIM"),
            Path::new("/sdcard/Movies/DCIM"),
            true,
        );

        assert!(fs.dir_cache.get(Path::new("/sdcard/DCIM")).is_none());
        assert!(fs.dir_cache.get(Path::new("/sdcard/DCIM/2024")).is_none());
        assert!(fs.dir_cache.get(Path::new("/sdcard/Movies")).is_none());
    }

    #[test]
    fn renaming_drops_the_stats_of_both_paths() {
        let fs = detached();
        fs.cache.put(PathBuf::from("/sdcard/old.bin"), stat_of(7));
        fs.cache.put(PathBuf::from("/sdcard/new.bin"), stat_of(7));

        fs.invalidate_after_rename(
            Path::new("/sdcard/old.bin"),
            Path::new("/sdcard/new.bin"),
            false,
        );

        assert!(fs.cache.get(Path::new("/sdcard/old.bin")).is_none());
        assert!(
            fs.cache.get(Path::new("/sdcard/new.bin")).is_none(),
            "the destination may have been overwritten"
        );
    }

    #[test]
    fn a_listing_served_from_the_cache_does_not_refresh_the_stats() {
        let fs = detached();
        let snapshot = listing(&["a.bin"]);
        fs.dir_cache.put(
            PathBuf::from("/sdcard"),
            Arc::clone(&snapshot),
            fs.dir_cache.epoch(),
        );
        // What a later lookup of the child found on the device.
        fs.cache.put(PathBuf::from("/sdcard/a.bin"), stat_of(4096));

        // A continued readdir off the cached snapshot: the entries are up to a
        // DIR_TTL old already, so their stats must not be cached afresh.
        fs.adopt_listing_entry(Path::new("/sdcard"), &snapshot[0], false);

        assert_eq!(
            fs.cache.get(Path::new("/sdcard/a.bin")).map(|s| s.size),
            Some(4096),
            "the stat cache kept the newer reading"
        );
    }

    #[test]
    fn a_freshly_listed_entry_is_cached_for_the_attributes_that_follow() {
        let fs = detached();
        let fresh = listing(&["a.bin"]);
        fs.dir_cache.put(
            PathBuf::from("/sdcard"),
            Arc::clone(&fresh),
            fs.dir_cache.epoch(),
        );

        let ino = fs.adopt_listing_entry(Path::new("/sdcard"), &fresh[0], true);

        assert!(
            ino > FUSE_ROOT_ID,
            "the entry was given an inode of its own"
        );
        assert_eq!(
            fs.cache.get(Path::new("/sdcard/a.bin")).map(|s| s.size),
            Some(0)
        );
        assert_eq!(
            fs.ino_to_path.lock().get(&ino),
            Some(&PathBuf::from("/sdcard/a.bin"))
        );
    }
}
