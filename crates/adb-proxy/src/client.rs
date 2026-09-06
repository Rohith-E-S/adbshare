//! Async client to the device-side proxy binary.
//!
//! Manages a pool of TCP connections to `127.0.0.1:<port>` (which the host has
//! forwarded to the device via `adb forward`). Each connection is a request/
//! response channel; the host issues RPCs and reads back status + data.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex as PlMutex;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, OwnedSemaphorePermit, Semaphore},
};

use crate::ops::{
    DirEntry, FileMode, OpenFlags, Op, Stat, Status,
};

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

    #[error("request timed out")]
    Timeout,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ProxyError>;

const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// Upper bound for a single RPC round-trip. Without it a hung device proxy
/// blocks FUSE ops (and transfers) forever.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
                if write_half.write_all(&msg).await.is_err() { break; }
                if write_half.flush().await.is_err() { break; }
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
                        Ok(0) => { *closed_r.lock() = true; return; }
                        Ok(_) => {}
                        Err(_) => { *closed_r.lock() = true; return; }
                    }
                }
                let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if len > MAX_RESPONSE {
                    *closed_r.lock() = true;
                    return;
                }
                while buf.len() < 4 + len {
                    match read_half.read_buf(&mut buf).await {
                        Ok(0) => { *closed_r.lock() = true; return; }
                        Ok(_) => {}
                        Err(_) => { *closed_r.lock() = true; return; }
                    }
                }
                let _len_bytes = buf.split_to(4);
                let body = buf.split_to(len).freeze();
                if resp_tx.send(body).await.is_err() { return; }
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

    /// Queue a pre-built frame synchronously (best effort, e.g. from Drop).
    /// The response, if any, is never consumed — callers must not reuse the
    /// connection afterwards.
    fn try_send_frame(&self, frame: Bytes) -> Result<()> {
        self.inner.write_tx.try_send(frame).map_err(|_| ProxyError::Closed)
    }

    /// Send a pre-built frame and wait for its response. Used by Drop so the
    /// connection stays synchronized after a best-effort close.
    async fn send_frame_and_await(&self, frame: Bytes) -> Result<Bytes> {
        if self.is_closed() || self.is_poisoned() { return Err(ProxyError::Closed); }
        let _guard = self.inner.req_lock.lock().await;
        let recv = async {
            self.inner.write_tx.send(frame).await.map_err(|_| ProxyError::Closed)?;
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
        if self.is_closed() || self.is_poisoned() { return Err(ProxyError::Closed); }
        // Serialize requests on this connection: lock held for the duration
        // of the request + response, so no two requests interleave.
        let _guard = self.inner.req_lock.lock().await;

        // Frame: [op u8][len u32 LE][args]
        let mut frame = BytesMut::with_capacity(5 + args.len());
        frame.extend_from_slice(&[op as u8]);
        frame.extend_from_slice(&(args.len() as u32).to_le_bytes());
        frame.extend_from_slice(args);

        // The reader task pushes every received frame onto `resp_tx` in
        // order. Because the connection is serialized by `req_lock`, the
        // next frame is ours.
        let recv = async {
            self.inner.write_tx.send(frame.freeze()).await
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
    pub fn addr(&self) -> &str { &self.addr }
    pub fn max_conns(&self) -> usize { self.max_conns }
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
        let permit = self.semaphore.clone().acquire_owned().await
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
        }.await;
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
        }.await;
        self.release(conn);
        res
    }

    pub async fn listdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::ListDir, &args).await?;
            parse_dir_entries(&resp)
        }.await;
        self.release(conn);
        res
    }

    pub async fn open(&self, path: &str, flags: OpenFlags, mode: u32) -> Result<ProxyFile> {
        let (conn, permit) = self.acquire().await?;
        let mut args = Vec::new();
        args.extend_from_slice(&flags.bits().to_le_bytes());
        args.extend_from_slice(&mode.to_le_bytes());
        args.extend_from_slice(&(path.len() as u32).to_le_bytes());
        args.extend_from_slice(path.as_bytes());
        let resp = match conn.request(Op::Open, &args).await {
            Ok(resp) => resp,
            Err(e) => { self.release(conn); return Err(e); }
        };
        if resp.len() < 4 {
            self.release(conn);
            return Err(ProxyError::Invalid("open response short".into()));
        }
        let fd = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
        Ok(ProxyFile {
            inner: Arc::new(ProxyFileInner {
                client: self.clone(),
                conn: PlMutex::new(Some(conn)),
                fd,
                path: path.to_string(),
                permit: PlMutex::new(Some(permit)),
            }),
        })
    }

    pub async fn mkdir(&self, path: &str, mode: u32) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&mode.to_le_bytes());
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            conn.request(Op::Mkdir, &args).await.map(|_| ())
        }.await;
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
        }.await;
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
        }.await;
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
        }.await;
        self.release(conn);
        res
    }

    pub async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&size.to_le_bytes());
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            conn.request(Op::Truncate, &args).await.map(|_| ())
        }.await;
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
        }.await;
        self.release(conn);
        res
    }

    pub async fn read_link(&self, path: &str) -> Result<String> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::ReadLink, &args).await?;
            String::from_utf8(resp.to_vec()).map_err(|_| ProxyError::Invalid("readlink utf8".into()))
        }.await;
        self.release(conn);
        res
    }

    pub async fn real_path(&self, path: &str) -> Result<String> {
        let (conn, _permit) = self.acquire().await?;
        let res = async {
            let mut args = Vec::new();
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path.as_bytes());
            let resp = conn.request(Op::RealPath, &args).await?;
            String::from_utf8(resp.to_vec()).map_err(|_| ProxyError::Invalid("realpath utf8".into()))
        }.await;
        self.release(conn);
        res
    }
}

/// A handle to a file opened on the device.
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
    /// Pool the connection is returned to when the file closes.
    client: ProxyClient,
    /// `None` once the file has been closed.
    conn: PlMutex<Option<ProxyConn>>,
    fd: u32,
    path: String,
    /// Pool permit held for the lifetime of the file: each open file counts
    /// against the client's concurrency limiter instead of silently removing
    /// a connection from the pool.
    permit: PlMutex<Option<OwnedSemaphorePermit>>,
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
    pub fn path(&self) -> &str { &self.inner.path }
    pub fn fd(&self) -> u32 { self.inner.fd }

    /// Clone of the live connection, or `Closed` if already closed.
    fn conn(&self) -> Result<ProxyConn> {
        self.inner.conn.lock().clone().ok_or(ProxyError::Closed)
    }

    pub async fn read_at(&self, offset: u64, len: u32) -> Result<Bytes> {
        let conn = self.conn()?;
        let mut args = Vec::new();
        args.extend_from_slice(&self.inner.fd.to_le_bytes());
        args.extend_from_slice(&offset.to_le_bytes());
        args.extend_from_slice(&len.to_le_bytes());
        conn.request(Op::Read, &args).await
    }

    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        let conn = self.conn()?;
        let mut args = Vec::new();
        args.extend_from_slice(&self.inner.fd.to_le_bytes());
        args.extend_from_slice(&offset.to_le_bytes());
        args.extend_from_slice(&(data.len() as u32).to_le_bytes());
        args.extend_from_slice(data);
        conn.request(Op::Write, &args).await.map(|_| ())
    }

    pub async fn close(self) -> Result<()> {
        let conn = self.inner.conn.lock().take().ok_or(ProxyError::Closed)?;
        let mut args = Vec::new();
        args.extend_from_slice(&self.inner.fd.to_le_bytes());
        let res = conn.request(Op::Close, &args).await.map(|_| ());
        // Return the connection to the pool (release() discards it if it is
        // closed/poisoned or the pool is full) and free the file's permit.
        self.inner.client.release(conn);
        drop(self.inner.permit.lock().take());
        res
    }
}

impl Drop for ProxyFile {
    fn drop(&mut self) {
        // Other clones may still be using the file handle; only the last
        // one out closes it. `conn` being `None` means close() already ran.
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        let conn = self.inner.conn.lock().take();
        let Some(conn) = conn else { return };
        let fd = self.inner.fd;
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
                // Drop can't await; spawn a task to send the Close properly
                // (consuming the response so the stream stays in sync) and
                // then recycle the connection into the pool.
                handle.spawn(async move {
                    let _ = conn.send_frame_and_await(close_frame).await;
                    client.release(conn);
                });
            }
            Err(_) => {
                // No runtime: best-effort synchronous send. The queued frame
                // still reaches the device before the socket closes, but the
                // response is never read, so the connection is discarded.
                let _ = conn.try_send_frame(close_frame);
            }
        }
    }
}

fn parse_dir_entries(data: &[u8]) -> Result<Vec<DirEntry>> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + 4 > data.len() { return Err(ProxyError::Invalid("dir entry name len".into())); }
        let name_len = u32::from_le_bytes([data[i], data[i+1], data[i+2], data[i+3]]) as usize;
        i += 4;
        if i + name_len + 60 > data.len() {
            return Err(ProxyError::Invalid("dir entry payload".into()));
        }
        let name = String::from_utf8(data[i..i+name_len].to_vec())
            .map_err(|_| ProxyError::Invalid("dir entry utf8".into()))?;
        i += name_len;
        let stat = Stat::decode(&data[i..i+60])
            .ok_or_else(|| ProxyError::Invalid("dir entry stat".into()))?;
        i += 60;
        entries.push(DirEntry { name, stat });
    }
    Ok(entries)
}

// Expose FileMode convenience constructors.
impl FileMode {
    pub fn dir() -> Self { Self(Self::S_IFDIR | 0o755) }
    pub fn file() -> Self { Self(Self::S_IFREG | 0o644) }
}
