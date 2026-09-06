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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    Request, FUSE_ROOT_ID,
};
use parking_lot::Mutex;
use thiserror::Error;

use adb_proxy::{FileMode, OpenFlags, ProxyClient, ProxyError, ProxyFile, Stat, Status};

use crate::cache::StatCache;

const TTL: Duration = Duration::from_secs(2);
const BLOCK_SIZE: u32 = 4096;
const ADB_UID: u32 = 2000;
const ADB_GID: u32 = 2000;

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
/// tokio current-thread runtime, and dispatches all proxy calls through
/// that runtime. The FUSE callback threads (which are NOT tokio
/// workers) call into this wrapper synchronously via `Handle::block_on`,
/// which the tokio runtime supports from any thread.
#[derive(Clone)]
struct SyncProxy {
    tx: tokio::sync::mpsc::UnboundedSender<ProxyRequest>,
}

type Reply<T> = std::sync::mpsc::Sender<std::result::Result<T, ProxyError>>;

enum ProxyRequest {
    Stat(String, Reply<Stat>),
    ListDir(String, Reply<Vec<adb_proxy::ops::DirEntry>>),
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
                let client = match rt.block_on(ProxyClient::connect(&addr, max_conns)) {
                    Ok(c) => std::sync::Arc::new(c),
                    Err(e) => {
                        eprintln!("adbfs: proxy connect to {addr} failed: {e}");
                        return;
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
        // The FUSE callback's `call` method uses a std::sync::mpsc to
        // dispatch requests synchronously. No tokio runtime needed.
        // Wait — we need async to wait on the oneshot. Use a blocking
        // recv with timeout? No, just use std_mpsc for the response.
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
    fn listdir(&self, path: &str) -> std::result::Result<Vec<adb_proxy::ops::DirEntry>, ProxyError> {
        self.call(|tx| ProxyRequest::ListDir(path.to_string(), tx))
    }
    fn open(&self, path: &str, flags: OpenFlags, mode: u32) -> std::result::Result<ProxyFile, ProxyError> {
        self.call(|tx| ProxyRequest::Open(path.to_string(), flags, mode, tx))
    }
    fn read_at(&self, file: ProxyFile, off: u64, len: u32) -> std::result::Result<bytes::Bytes, ProxyError> {
        self.call(|tx| ProxyRequest::ReadAt(file, off, len, tx))
    }
    fn write_at(&self, file: ProxyFile, off: u64, data: Vec<u8>) -> std::result::Result<(), ProxyError> {
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
    pub fn new(client: ProxyClient, _rt: tokio::runtime::Handle) -> Self {
        let proxy = SyncProxy::start(client);
        let mut ino_to_path = HashMap::new();
        let mut path_to_ino = HashMap::new();
        ino_to_path.insert(FUSE_ROOT_ID, PathBuf::from("/"));
        path_to_ino.insert(PathBuf::from("/"), FUSE_ROOT_ID);
        Self {
            proxy,
            cache: StatCache::new(TTL),
            ino_to_path: Mutex::new(ino_to_path),
            path_to_ino: Mutex::new(path_to_ino),
            next_ino: AtomicU64::new(100),
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    fn ino_for(&self, path: PathBuf) -> u64 {
        let mut p2i = self.path_to_ino.lock();
        if let Some(&ino) = p2i.get(&path) { return ino; }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        p2i.insert(path.clone(), ino);
        self.ino_to_path.lock().insert(ino, path);
        ino
    }

    fn attr_from_stat(&self, ino: u64, stat: Stat) -> FileAttr {
        let kind = if stat.mode.is_dir() {
            FileType::Directory
        } else if stat.mode.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
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

    fn resolve_child(&self, parent: u64, name: &OsStr) -> Option<PathBuf> {
        let parent_path = self.ino_to_path.lock().get(&parent).cloned()?;
        let mut p = parent_path;
        p.push(name);
        Some(p)
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
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.stat(&path_str) {
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
            None => { reply.error(libc::EINVAL); return; }
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
        match self.proxy.stat(&path_str) {
            Ok(stat) => {
                self.cache.put(path, stat);
                let attr = self.attr_from_stat(ino, stat);
                reply.attr(&TTL, &attr);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let path = self.ino_to_path.lock().get(&ino).cloned();
        let path = match path {
            Some(p) => p,
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let entries = self.proxy.listdir(&path_str);
        let entries = match entries {
            Ok(e) => e,
            Err(e) => { reply.error(Self::proxy_to_errno(e)); return; }
        };
        let mut cur = offset.max(0) as usize;
        if cur == 0 { let _ = reply.add(ino, 1, FileType::Directory, "."); cur = 1; }
        if cur == 1 { let _ = reply.add(FUSE_ROOT_ID, 2, FileType::Directory, ".."); cur = 2; }
        for (n, entry) in entries.into_iter().enumerate().skip(cur.saturating_sub(2)) {
            let child_path = {
                let mut p = path.clone();
                p.push(OsStr::from_bytes(entry.name.as_bytes()));
                p
            };
            let child_ino = self.ino_for(child_path.clone());
            self.cache.put(child_path, entry.stat);
            let kind = if entry.stat.mode.is_dir() { FileType::Directory } else { FileType::RegularFile };
            let _ = reply.add(child_ino, (n as i64) + 3, kind, entry.name);
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let path = self.ino_to_path.lock().get(&ino).cloned();
        let path = match path {
            Some(p) => p,
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let mut oflags = OpenFlags::READ;
        if flags & libc::O_WRONLY != 0 { oflags = OpenFlags::WRITE; }
        if flags & libc::O_RDWR != 0 { oflags = OpenFlags::READ | OpenFlags::WRITE; }
        if flags & libc::O_CREAT != 0 { oflags |= OpenFlags::CREATE; }
        if flags & libc::O_TRUNC != 0 { oflags |= OpenFlags::TRUNC; }
        if flags & libc::O_APPEND != 0 { oflags |= OpenFlags::APPEND; }
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

    fn read(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyData) {
        let file = { self.open_files.lock().get(&fh).map(|f| f.proxy.clone()) };
        let file = match file {
            Some(f) => f,
            None => { reply.error(libc::EBADF); return; }
        };
        let res = self.proxy.read_at(file, offset as u64, size);
        match res {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn write(&mut self, _req: &Request<'_>, ino: u64, fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyWrite) {
        let file = { self.open_files.lock().get(&fh).map(|f| f.proxy.clone()) };
        let file = match file {
            Some(f) => f,
            None => { reply.error(libc::EBADF); return; }
        };
        let res = self.proxy.write_at(file, offset as u64, data.to_vec());
        match res {
            Ok(()) => {
                if let Some(path) = self.ino_to_path.lock().get(&ino) {
                    self.cache.invalidate(path);
                }
                reply.written(data.len() as u32);
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn release(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _flags: i32, _lock_owner: Option<u64>, _flush: bool, reply: ReplyEmpty) {
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
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        let mut oflags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
        if flags & libc::O_TRUNC != 0 { oflags |= OpenFlags::TRUNC; }
        if flags & libc::O_EXCL != 0 { oflags |= OpenFlags::EXCL; }
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
            None => { reply.error(libc::EINVAL); return; }
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
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.unlink(&path_str) {
            Ok(()) => {
                self.cache.invalidate(&path);
                reply.ok();
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.resolve_child(parent, name) {
            Some(p) => p,
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.rmdir(&path_str) {
            Ok(()) => {
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
            None => { reply.error(libc::EINVAL); return; }
        };
        let dst = match self.resolve_child(newparent, newname) {
            Some(p) => p,
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(src_str) = path_to_string(&src) else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some(dst_str) = path_to_string(&dst) else {
            reply.error(libc::EINVAL);
            return;
        };
        match self.proxy.rename(&src_str, &dst_str) {
            Ok(()) => {
                self.cache.invalidate(&src);
                self.cache.invalidate(&dst);
                reply.ok();
            }
            Err(e) => reply.error(Self::proxy_to_errno(e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
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
            None => { reply.error(libc::EINVAL); return; }
        };
        let Some(path_str) = path_to_string(&path) else {
            reply.error(libc::EINVAL);
            return;
        };
        if let Some(s) = size {
            let _ = self.proxy.truncate(&path_str, s);
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
        reply.statfs(1 << 30, 1 << 20, 1 << 20, 1 << 10, 1 << 10, BLOCK_SIZE, 256, 4096);
    }
}
