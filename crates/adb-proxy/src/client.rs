//! Async client to the device-side proxy binary.
//!
//! Manages a pool of TCP connections to `127.0.0.1:<port>` (which the host has
//! forwarded to the device via `adb forward`). Each connection is a request/
//! response channel; the host issues RPCs and reads back status + data.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex as PlMutex;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
};

use crate::ops::{DirEntry, FileMode, Op, OpenFlags, Stat, Status};

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("connection closed")]
    Closed,

    #[error("response too large ({0} bytes)")]
    TooLarge(usize),

    #[error("server error: {0:?} {1}")]
    Status(Status, String),

    #[error("invalid response: {0}")]
    Invalid(String),

    #[error("connection pool exhausted")]
    PoolExhausted,

    /// No pooled connection became free in time. Distinct from `Timeout`:
    /// this says nothing about the device, only that every connection is
    /// already busy with another request.
    #[error("no free connection available (pool busy)")]
    Busy,

    #[error("request timed out")]
    Timeout,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;

/// Largest *payload* in a single reply.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// Ceiling on the whole reply frame, which carries a one-byte status in front
/// of the payload.
///
/// The reader compared the frame length against `MAX_RESPONSE`, so the one
/// value the device will actually send largest — a read of exactly
/// `MAX_RESPONSE` bytes, which it explicitly permits — arrived as
/// `MAX_RESPONSE + 1` and was treated as an oversized frame: the connection
/// was closed and the data silently dropped. Measured against the device
/// proxy: a read of `MAX_RESPONSE - 1` came back intact, `MAX_RESPONSE`
/// closed the connection, and `MAX_RESPONSE + 1` was refused by the device
/// with a `too big` status. Nothing could ever transfer an 8 MiB chunk.
const MAX_RESPONSE_FRAME: usize = MAX_RESPONSE + 1;

/// Largest request body we will put on the wire. Matches the device proxy's
/// own `MAX_REQUEST`. Enforced here so an oversized write fails as
/// `TooLarge` instead of being silently dropped by the device — which
/// surfaces as "connection closed" and takes a pooled connection with it.
/// fuser advertises `max_write` of 16 MiB, so this is genuinely reachable
/// from a large FUSE write.
const MAX_REQUEST: usize = 8 * 1024 * 1024;

/// Upper bound for a single RPC round-trip. Without it a hung device proxy
/// blocks FUSE ops (and transfers) forever.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Upper bound on waiting for a free pooled connection. Permits are held for
/// the duration of one round trip, so a saturated pool means every connection
/// is mid-request — a parallel transfer batch, or a file manager loading many
/// previews at once. It must not block forever: the FUSE event loop and the
/// D-Bus handlers wait here with nothing else to bound them.
const POOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How many times a dropped `ProxyFile` retries borrowing a connection to send
/// its `Op::Close`. Sends are best-effort from `Drop`, but an abandoned fd
/// accumulates on the device proxy for as long as it runs.
const CLOSE_RETRIES: usize = 4;

/// A single TCP connection to the proxy. Cheap to clone (it's an Arc).
#[derive(Clone)]
pub struct ProxyConn {
    inner: Arc<ProxyConnInner>,
}

impl ProxyConn {
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.lock()
    }

    /// True once a request on this connection timed out: the peer may still
    /// deliver a stale response that would desynchronize the next request,
    /// so the connection must be discarded rather than pooled.
    pub fn is_poisoned(&self) -> bool {
        *self.inner.poisoned.lock()
    }
}

impl std::fmt::Debug for ProxyConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConn").finish()
    }
}

struct ProxyConnInner {
    write_tx: mpsc::Sender<Bytes>,
    read_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Bytes>>>,
    closed: Arc<PlMutex<bool>>,
    poisoned: Arc<PlMutex<bool>>,
    /// Serializes requests on this connection: each request must wait for
    /// the previous response before sending the next, because the protocol
    /// is strictly request/response with no IDs.
    req_lock: tokio::sync::Mutex<()>,
}

impl ProxyConn {
    /// Open a new connection to the proxy. Caller is responsible for not
    /// holding more than a few hundred of these — they're 1:1 with TCP sockets.
    pub async fn open(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (read_half, mut write_half) = stream.into_split();
        let (write_tx, mut write_rx) = mpsc::channel::<Bytes>(64);
        let (resp_tx, resp_rx) = mpsc::channel::<Bytes>(64);
        let closed = Arc::new(PlMutex::new(false));
        let closed_w = closed.clone();
        let closed_r = closed.clone();
        let poisoned = Arc::new(PlMutex::new(false));

        // Writer task: drains write_tx into the TCP socket.
        tokio::spawn(async move {
            while let Some(msg) = write_rx.recv().await {
                if write_half.write_all(&msg).await.is_err() {
                    break;
                }
                if write_half.flush().await.is_err() {
                    break;
                }
            }
            *closed_w.lock() = true;
        });

        // Reader task: parses length-prefixed frames and routes them to resp_tx.
        // Wire format: [length u32 LE][status u8][body bytes]
        tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(64 * 1024);
            let mut read_half = read_half;
            loop {
                // Need at least 4 bytes for the length prefix.
                while buf.len() < 4 {
                    match read_half.read_buf(&mut buf).await {
                        Ok(0) => {
                            *closed_r.lock() = true;
                            return;
                        }
                        Ok(_) => {}
                        Err(_) => {
                            *closed_r.lock() = true;
                            return;
                        }
                    }
                }
                let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if len > MAX_RESPONSE_FRAME {
                    *closed_r.lock() = true;
                    return;
                }
                while buf.len() < 4 + len {
                    match read_half.read_buf(&mut buf).await {
                        Ok(0) => {
                            *closed_r.lock() = true;
                            return;
                        }
                        Ok(_) => {}
                        Err(_) => {
                            *closed_r.lock() = true;
                            return;
                        }
                    }
                }
                let _len_bytes = buf.split_to(4);
                let body = buf.split_to(len).freeze();
                if resp_tx.send(body).await.is_err() {
                    return;
                }
            }
        });

        Ok(Self {
            inner: Arc::new(ProxyConnInner {
                write_tx,
                read_rx: Arc::new(tokio::sync::Mutex::new(resp_rx)),
                closed,
                poisoned,
                req_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Send a pre-built frame and wait for its response. Used by Drop so the
    /// connection stays synchronized after a best-effort close.
    async fn send_frame_and_await(&self, frame: Bytes) -> Result<Bytes> {
        if self.is_closed() || self.is_poisoned() {
            return Err(ProxyError::Closed);
        }
        let _guard = self.inner.req_lock.lock().await;
        let recv = async {
            self.inner
                .write_tx
                .send(frame)
                .await
                .map_err(|_| ProxyError::Closed)?;
            let mut rx = self.inner.read_rx.lock().await;
            rx.recv().await.ok_or(ProxyError::Closed)
        };
        let resp = match tokio::time::timeout(REQUEST_TIMEOUT, recv).await {
            Ok(resp) => resp?,
            Err(_) => {
                *self.inner.poisoned.lock() = true;
                *self.inner.closed.lock() = true;
                return Err(ProxyError::Timeout);
            }
        };
        if resp.is_empty() {
            return Err(ProxyError::Invalid("empty response frame".into()));
        }
        let status = Status::from_u8(resp[0]);
        let data = resp.slice(1..);
        if status != Status::Ok {
            let s = String::from_utf8_lossy(&data).into_owned();
            return Err(ProxyError::Status(status, s));
        }
        Ok(data)
    }

    /// Issue a request and wait for the response. The request payload is
    /// the `args` part; the response payload is the data section.
    async fn request(&self, op: Op, args: &[u8]) -> Result<Bytes> {
        if self.is_closed() || self.is_poisoned() {
            return Err(ProxyError::Closed);
        }
        // Check before touching the socket: the device cannot recover from an
        // oversized frame, and sending it would poison this connection.
        if args.len() > MAX_REQUEST {
            return Err(ProxyError::TooLarge(args.len()));
        }
        // The length field is 32-bit; a longer slice would frame as a bogus
        // size rather than fail.
        let Ok(len) = u32::try_from(args.len()) else {
            return Err(ProxyError::TooLarge(args.len()));
        };
        // Serialize requests on this connection: lock held for the duration
        // of the request + response, so no two requests interleave.
        let _guard = self.inner.req_lock.lock().await;

        // Frame: [op u8][len u32 LE][args]
        let mut frame = BytesMut::with_capacity(5 + args.len());
        frame.extend_from_slice(&[op as u8]);
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(args);

        // The reader task pushes every received frame onto `resp_tx` in
        // order. Because the connection is serialized by `req_lock`, the
        // next frame is ours.
        let recv = async {
            self.inner
                .write_tx
                .send(frame.freeze())
                .await
                .map_err(|_| ProxyError::Closed)?;
            let mut rx = self.inner.read_rx.lock().await;
            rx.recv().await.ok_or(ProxyError::Closed)
        };
        let resp = match tokio::time::timeout(REQUEST_TIMEOUT, recv).await {
            Ok(resp) => resp?,
            Err(_) => {
                // A caller abandoning a timed-out request would leave a
                // stale response in flight, which the next request on this
                // connection would consume. Mark the connection unusable so
                // release() discards it instead of pooling it.
                *self.inner.poisoned.lock() = true;
                *self.inner.closed.lock() = true;
                return Err(ProxyError::Timeout);
            }
        };
        if resp.is_empty() {
            // A zero-length frame carries no status byte; indexing resp[0]
            // would panic (and with panic=abort take the whole daemon down).
            return Err(ProxyError::Invalid("empty response frame".into()));
        }
        let status = Status::from_u8(resp[0]);
        let data = resp.slice(1..);
        if status != Status::Ok {
            let s = String::from_utf8_lossy(&data).into_owned();
            return Err(ProxyError::Status(status, s));
        }
        Ok(data)
    }
}

/// Pool of connections to a single proxy.
#[derive(Clone)]
pub struct ProxyClient {
    addr: String,
    pool: Arc<PlMutex<Vec<ProxyConn>>>,
    semaphore: Arc<Semaphore>,
    max_conns: usize,
}

impl ProxyClient {
    pub fn addr(&self) -> &str {
        &self.addr
    }
    pub fn max_conns(&self) -> usize {
        self.max_conns
    }
}

impl std::fmt::Debug for ProxyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyClient")
            .field("addr", &self.addr)
            .field("max_conns", &self.max_conns)
            .finish()
    }
}

impl ProxyClient {
    pub async fn connect(addr: impl Into<String>, max_conns: usize) -> Result<Self> {
        let addr = addr.into();
        let mut pool = Vec::with_capacity(max_conns);
        for _ in 0..max_conns {
            pool.push(ProxyConn::open(&addr).await?);
        }
        Ok(Self {
            addr,
            pool: Arc::new(PlMutex::new(pool)),
            semaphore: Arc::new(Semaphore::new(max_conns)),
            max_conns,
        })
    }

    async fn acquire(&self) -> Result<(ProxyConn, OwnedSemaphorePermit)> {
        let permit = tokio::time::timeout(POOL_TIMEOUT, self.semaphore.clone().acquire_owned())
            .await
            .map_err(|_| ProxyError::Busy)?
            .map_err(|_| ProxyError::PoolExhausted)?;
        loop {
            let conn = {
                let mut pool = self.pool.lock();
                pool.pop()
            };
            match conn {
                Some(c) if !c.is_closed() && !c.is_poisoned() => return Ok((c, permit)),
                Some(_) => {
                    // Closed connection popped; try opening a replacement.
                    match ProxyConn::open(&self.addr).await {
                        Ok(fresh) => return Ok((fresh, permit)),
                        Err(_) => continue,
                    }
                }
                None => {
                    // Pool is empty; open fresh connection.
                    let fresh = ProxyConn::open(&self.addr).await?;
                    return Ok((fresh, permit));
                }
            }
        }
    }

    fn release(&self, conn: ProxyConn) {
        if conn.is_closed() || conn.is_poisoned() {
            // A timed-out request may leave a stale response in flight, so
            // a poisoned connection can never be reused safely.
            return;
        }
        let mut pool = self.pool.lock();
        if pool.len() < self.max_conns {
            pool.push(conn);
        }
    }

    pub async fn stat(&self, path: &str) -> Result<Stat> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::Stat, &args).await?;
            Stat::decode(&resp).ok_or_else(|| ProxyError::Invalid("stat decode".into()))
        }
        .await;
        self.release(conn);
        res
    }

    pub async fn lstat(&self, path: &str) -> Result<Stat> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::Lstat, &args).await?;
            Stat::decode(&resp).ok_or_else(|| ProxyError::Invalid("lstat decode".into()))
        }
        .await;
        self.release(conn);
        res
    }

    /// Create a symlink at `link` pointing to `target`.
    ///
    /// The device op carries two paths, both raw: a link target is not
    /// required to be UTF-8 either, and the protocol already transmits the
    /// bytes, so they are sent unchanged.
    pub async fn symlink(&self, target: &Path, link: &Path) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let (t, l) = (target.as_os_str().as_bytes(), link.as_os_str().as_bytes());
            if t.len() > MAX_REQUEST || l.len() > MAX_REQUEST {
                return Err(ProxyError::TooLarge(t.len().max(l.len())));
            }
            let mut args = Vec::with_capacity(8 + t.len() + l.len());
            args.extend_from_slice(&(t.len() as u32).to_le_bytes());
            args.extend_from_slice(t);
            args.extend_from_slice(&(l.len() as u32).to_le_bytes());
            args.extend_from_slice(l);
            conn.request(Op::Symlink, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    /// Target of a symlink, as raw bytes: a link target need not be UTF-8.
    pub async fn readlink(&self, path: &str) -> Result<Vec<u8>> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::ReadLink, &args).await?;
            // Payload is [len u32][target]; the declared length is what the
            // device wrote, so trust the buffer only as far as it agrees.
            if resp.len() < 4 {
                return Err(ProxyError::Invalid("readlink decode".into()));
            }
            let n = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]) as usize;
            match resp.get(4..4 + n) {
                Some(target) => Ok(target.to_vec()),
                None => Err(ProxyError::Invalid("readlink length".into())),
            }
        }
        .await;
        self.release(conn);
        res
    }

    /// Entries requested per `ListDir` round trip. Must be <= the device's
    /// `LISTDIR_PAGE`; a bigger request is served as a full page and the loop
    /// simply takes more trips.
    const LISTDIR_PAGE: u32 = 16384;

    /// Whole directory, paging until the device reports it has nothing left.
    ///
    /// The device sends one page per request: it used to send the entire
    /// listing in one frame, so a directory past the frame cap (~118k entries)
    /// made the host close the connection and the directory became unlistable.
    pub async fn listdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let mut all = Vec::new();
        let mut offset = 0u32;
        loop {
            let page = self.listdir_page(path, offset).await?;
            let got = page.len() as u32;
            all.extend(page);
            // A short page means the directory ended. `offset` advances by the
            // number the device *emitted*, which is what its own skip counter
            // counts, so entries it could not lstat do not desynchronise the
            // walk.
            if got < Self::LISTDIR_PAGE {
                return Ok(all);
            }
            offset += got;
        }
    }

    async fn listdir_page(&self, path: &str, offset: u32) -> Result<Vec<DirEntry>> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            args.extend_from_slice(&offset.to_le_bytes());
            args.extend_from_slice(&Self::LISTDIR_PAGE.to_le_bytes());
            let resp = conn.request(Op::ListDir, &args).await?;
            parse_dir_page(&resp)
        }
        .await;
        self.release(conn);
        res
    }

    pub async fn open(&self, path: &str, flags: OpenFlags, mode: u32) -> Result<ProxyFile> {
        let (conn, _permit) = self.acquire().await?;
        let mut args = Vec::new();
        args.extend_from_slice(&flags.bits().to_le_bytes());
        args.extend_from_slice(&mode.to_le_bytes());
        args.extend_from_slice(&(path.len() as u32).to_le_bytes());
        args.extend_from_slice(path.as_bytes());
        let resp = match conn.request(Op::Open, &args).await {
            Ok(resp) => resp,
            Err(e) => {
                self.release(conn);
                return Err(e);
            }
        };
        if resp.len() < 4 {
            self.release(conn);
            return Err(ProxyError::Invalid("open response short".into()));
        }
        let fd = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        // Hand the connection back. `ProxyFile` no longer holds one, so
        // dropping it here without releasing would shrink the pool by one on
        // every open — after `max_conns` opens the pool is empty and each
        // acquire dials a fresh connection to the device.
        self.release(conn);
        Ok(ProxyFile {
            inner: Arc::new(ProxyFileInner {
                client: self.clone(),
                fd: PlMutex::new(Some(fd)),
                path: path.to_string(),
            }),
        })
    }

    pub async fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            args.extend_from_slice(&mode.to_le_bytes());
            conn.request(Op::Mkdir, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    pub async fn unlink(&self, path: &str) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            conn.request(Op::Unlink, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    pub async fn rmdir(&self, path: &str) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            conn.request(Op::Rmdir, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    pub async fn rename(&self, src: &str, dst: &str) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(src.len() as u32).to_le_bytes());
            args.extend_from_slice(src.as_bytes());
            args.extend_from_slice(&(dst.len() as u32).to_le_bytes());
            args.extend_from_slice(dst.as_bytes());
            conn.request(Op::Rename, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    pub async fn copy_file(&self, src: &str, dst: &str) -> Result<()> {
        for path in [src, dst] {
            if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
                return Err(ProxyError::Status(
                    Status::InvalidArg,
                    "copy paths must be absolute, non-NUL, and at most 4096 bytes".into(),
                ));
            }
        }
        let (conn, _permit) = self.acquire().await?;
        let mut args = Vec::new();
        args.extend_from_slice(&(src.len() as u32).to_le_bytes());
        args.extend_from_slice(src.as_bytes());
        args.extend_from_slice(&(dst.len() as u32).to_le_bytes());
        args.extend_from_slice(dst.as_bytes());
        let res = conn.request(Op::CopyFile, &args).await.map(|_| ()).map_err(
            |error| match error {
                ProxyError::Status(_, _) => error,
                _ => ProxyError::Other(format!(
                    "{error}; completion unknown; destination may be incomplete or still copying"
                )),
            },
        );
        self.release(conn);
        res
    }

    pub async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            args.extend_from_slice(&size.to_le_bytes());
            conn.request(Op::Truncate, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    /// Set the access/modification times of `path` (unix epoch seconds).
    /// Wire format: [path len u32][path][atime i64 LE][mtime i64 LE].
    pub async fn utime(&self, path: &str, atime: i64, mtime: i64) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            args.extend_from_slice(&atime.to_le_bytes());
            args.extend_from_slice(&mtime.to_le_bytes());
            conn.request(Op::Utime, &args).await.map(|_| ())
        }
        .await;
        self.release(conn);
        res
    }

    /// Filesystem usage for the mount containing `path` (op `DiskUsage` /
    /// `statvfs` on the device). Returns available + total bytes.
    pub async fn disk_usage(&self, path: &str) -> Result<DiskUsage> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::DiskUsage, &args).await?;
            DiskUsage::decode(&resp)
        }
        .await;
        self.release(conn);
        res
    }
}

/// Filesystem usage in bytes (from the device `statvfs` handler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub avail_bytes: u64,
    pub total_bytes: u64,
}

impl DiskUsage {
    /// Decode the `DiskUsage` response body: `[avail_blocks][blocks][bsize]`
    /// as u64 LE (64-bit device) or u32 LE (32-bit device).
    fn decode(data: &[u8]) -> Result<Self> {
        if data.len() >= 24 {
            let avail = u64::from_le_bytes(data[0..8].try_into().unwrap());
            let total = u64::from_le_bytes(data[8..16].try_into().unwrap());
            let bsize = u64::from_le_bytes(data[16..24].try_into().unwrap());
            Ok(Self {
                avail_bytes: avail.saturating_mul(bsize),
                total_bytes: total.saturating_mul(bsize),
            })
        } else if data.len() >= 12 {
            let avail = u32::from_le_bytes(data[0..4].try_into().unwrap()) as u64;
            let total = u32::from_le_bytes(data[4..8].try_into().unwrap()) as u64;
            let bsize = u32::from_le_bytes(data[8..12].try_into().unwrap()) as u64;
            Ok(Self {
                avail_bytes: avail.saturating_mul(bsize),
                total_bytes: total.saturating_mul(bsize),
            })
        } else {
            Err(ProxyError::Invalid("diskusage decode".into()))
        }
    }
}

/// A handle to a file opened on the device.
///
/// The device-side fd is process-global state, so *any* pooled connection can
/// service a read/write against it — `Op::Read`/`Op::Write` carry the fd and
/// offset in the request. This handle therefore does **not** reserve a pooled
/// connection for its lifetime; each operation borrows one for the duration
/// of the round trip. Reserving one per open file caps concurrent opens at
/// the pool size, which deadlocks a FUSE mount: the fifth `open` waits for a
/// permit that only the `release` of the first four could return, and on a
/// single event-loop thread that never arrives.
///
/// Cloning shares the same underlying file handle: the `Op::Close` is sent
/// by `close()` or, failing that, when the *last* clone is dropped. Every
/// error path after `open` therefore releases the device-side fd instead of
/// leaking it until EMFILE.
#[derive(Clone)]
pub struct ProxyFile {
    inner: Arc<ProxyFileInner>,
}

struct ProxyFileInner {
    /// Pool that connections are borrowed from and returned to.
    client: ProxyClient,
    /// Device-side fd, or `None` once the file has been closed.
    fd: PlMutex<Option<u32>>,
    path: String,
}

impl std::fmt::Debug for ProxyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyFile")
            .field("fd", &self.inner.fd)
            .field("path", &self.inner.path)
            .finish()
    }
}

impl ProxyFile {
    /// The device-side fd, or `Closed` if this handle has been closed.
    fn fd(&self) -> Result<u32> {
        self.inner.fd.lock().ok_or(ProxyError::Closed)
    }

    pub async fn read_at(&self, offset: u64, len: u32) -> Result<Bytes> {
        let fd = self.fd()?;
        let (conn, _permit) = self.inner.client.acquire().await?;
        let mut args = Vec::new();
        args.extend_from_slice(&fd.to_le_bytes());
        args.extend_from_slice(&offset.to_le_bytes());
        args.extend_from_slice(&len.to_le_bytes());
        let res = conn.request(Op::Read, &args).await;
        self.inner.client.release(conn);
        res
    }

    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        let fd = self.fd()?;
        let (conn, _permit) = self.inner.client.acquire().await?;
        let mut args = Vec::new();
        args.extend_from_slice(&fd.to_le_bytes());
        args.extend_from_slice(&offset.to_le_bytes());
        args.extend_from_slice(&(data.len() as u32).to_le_bytes());
        args.extend_from_slice(data);
        let res = conn.request(Op::Write, &args).await.map(|_| ());
        self.inner.client.release(conn);
        res
    }

    /// `fstat` on this handle's device-side fd.
    ///
    /// Describes the file that is actually open, which a path-based `stat`
    /// cannot do once the file has been unlinked or replaced: the fd keeps
    /// referring to the original inode while the path now resolves to
    /// something else entirely.
    pub async fn stat(&self) -> Result<Stat> {
        let fd = self.fd()?;
        let (conn, _permit) = self.inner.client.acquire().await?;
        let res = async {
            let resp = conn.request(Op::Fstat, &fd.to_le_bytes()).await?;
            Stat::decode(&resp).ok_or_else(|| ProxyError::Invalid("fstat decode".into()))
        }
        .await;
        self.inner.client.release(conn);
        res
    }

    pub async fn close(self) -> Result<()> {
        // Borrow a connection *before* claiming the fd. Claiming first and then
        // hitting `?` on the borrow is what leaked: the fd would already be out
        // of `inner.fd`, so `Drop` would skip it, `self` is consumed so nothing
        // can retry, and the device-side fd is orphaned for the life of the
        // device proxy process.
        let (conn, _permit) = self.inner.client.acquire().await?;
        // Still claim before sending, so a concurrent clone cannot close it too.
        let Some(fd) = self.inner.fd.lock().take() else {
            self.inner.client.release(conn);
            return Err(ProxyError::Closed);
        };
        let mut args = Vec::new();
        args.extend_from_slice(&fd.to_le_bytes());
        let res = conn.request(Op::Close, &args).await.map(|_| ());
        self.inner.client.release(conn);
        res
    }
}

impl Drop for ProxyFile {
    fn drop(&mut self) {
        // Other clones may still be using the file handle; only the last
        // one out closes it. `fd` being `None` means close() already ran.
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        let Some(fd) = self.inner.fd.lock().take() else {
            return;
        };
        let client = self.inner.client.clone();

        // Build the Close request frame: [op u8][len u32 LE][fd u32 LE].
        let close_frame = {
            let mut frame = BytesMut::with_capacity(5 + 4);
            frame.extend_from_slice(&[Op::Close as u8]);
            frame.extend_from_slice(&4u32.to_le_bytes());
            frame.extend_from_slice(&fd.to_le_bytes());
            frame.freeze()
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Drop can't await; spawn a task to borrow a connection, send
                // the Close properly (consuming the response so the stream
                // stays in sync) and return the connection to the pool.
                //
                // `Op::Close` is the only way to release this fd, and nothing on
                // the device side will do it for us — closing a connection's
                // fds on teardown would break live handles, since a ProxyFile
                // outlives the connection it was opened over. So a saturated
                // pool has to be waited out rather than shrugged off: retry a
                // bounded number of times, then give up and say so.
                handle.spawn(async move {
                    for attempt in 0..CLOSE_RETRIES {
                        match client.acquire().await {
                            Ok((conn, _permit)) => {
                                let _ = conn.send_frame_and_await(close_frame).await;
                                client.release(conn);
                                return;
                            }
                            Err(ProxyError::Busy) if attempt + 1 < CLOSE_RETRIES => {
                                tracing::debug!(attempt, "pool busy sending Op::Close; retrying");
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    100 * (attempt as u64 + 1),
                                ))
                                .await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = ?e,
                                    "could not send Op::Close; the device-side fd is leaked"
                                );
                                return;
                            }
                        }
                    }
                });
            }
            Err(_) => {
                // No runtime, so nothing can be sent. The caller is on a
                // thread with no executor; the fd is leaked and only a restart
                // of the device proxy reclaims it.
                tracing::debug!("no runtime to send Op::Close on; device-side fd is leaked");
                drop(close_frame);
            }
        }
    }
}

/// One `ListDir` page: `[count u32]` then that many entries. The count is
/// cross-checked against the buffer so a truncated page is rejected instead of
/// silently yielding a short directory.
fn parse_dir_page(data: &[u8]) -> Result<Vec<DirEntry>> {
    if data.len() < 4 {
        return Err(ProxyError::Invalid("listdir count".into()));
    }
    let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    parse_dir_entries(&data[4..], count)
}

fn parse_dir_entries(data: &[u8], expect: usize) -> Result<Vec<DirEntry>> {
    let mut entries = Vec::with_capacity(expect.min(4096));
    let mut i = 0;
    while i < data.len() {
        if i + 4 > data.len() {
            return Err(ProxyError::Invalid("dir entry name len".into()));
        }
        let name_len =
            u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if i + name_len + 60 > data.len() {
            return Err(ProxyError::Invalid("dir entry payload".into()));
        }
        // Linux filenames are arbitrary byte strings, and Android media folders are a
        // common home for Shift-JIS/CP437 names that are not valid UTF-8. Decoding
        // strictly made one such name fail the whole listing — the directory became
        // unlistable (EIO on FUSE readdir) and the daemon's recursive copy failed on
        // it. Decode lossily instead, so a bad name renders as U+FFFD and its
        // neighbours are still usable. Same rationale as the GUI's own
        // `to_string_lossy` on paths.
        let name = String::from_utf8_lossy(&data[i..i + name_len]).into_owned();
        i += name_len;
        let stat = Stat::decode(&data[i..i + 60])
            .ok_or_else(|| ProxyError::Invalid("dir entry stat".into()))?;
        i += 60;
        entries.push(DirEntry { name, stat });
    }
    if entries.len() != expect {
        return Err(ProxyError::Invalid(format!(
            "listdir page: header said {expect} entries, buffer held {}",
            entries.len()
        )));
    }
    Ok(entries)
}

// Expose FileMode convenience constructors.
impl FileMode {
    pub fn dir() -> Self {
        Self(Self::S_IFDIR | 0o755)
    }
    pub fn file() -> Self {
        Self(Self::S_IFREG | 0o644)
    }
    /// Symlinks are always mode 0777 on Linux; the kernel ignores the
    /// permission bits for them.
    pub fn symlink() -> Self {
        Self(Self::S_IFLNK | 0o777)
    }
}
