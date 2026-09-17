use std::env;
use std::ffi::CString;
use std::process::ExitCode;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MAX_REQUEST: usize = 8 * 1024 * 1024;
const MAX_PATH: usize = 4096;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const LOG_PATH: &str = "/data/local/tmp/adbshare-proxy.log";
/// Close a client connection after this much time without a completed
/// request, so a stalled client cannot pin a task + socket forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

fn wlog(msg: String) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().append(true).create(true).open(LOG_PATH) {
        // Force the file to flush + sync so the log reaches disk even on
        // a panic / abort.
        let _ = writeln!(f, "{}", msg);
        let _ = f.flush();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let port: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(31337);

    let _ = std::fs::File::create(LOG_PATH);
    // Bind loopback only: the sole intended client is `adb forward`, which
    // connects to this port via localhost *on the device*. Binding a public
    // interface would expose an unauthenticated file RPC to every host on
    // the phone's network.
    wlog(format!("proxy starting on 127.0.0.1:{}", port));

    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            wlog(format!("bind 127.0.0.1:{}: {}", port, e));
            return ExitCode::from(1);
        }
    };
    wlog(format!("listening on 127.0.0.1:{} (loopback only)", port));

    loop {
        wlog("[main] waiting for connection".to_string());
        match listener.accept().await {
            Ok((stream, addr)) => {
                wlog(format!("accepted from {:?}", addr));
                tokio::spawn(handle_connection(stream, addr));
            }
            Err(e) => wlog(format!("accept: {}", e)),
        }
    }
}

/// Arms a watchdog that force-closes the client socket if no request is
/// completed within `IDLE_TIMEOUT`. Sending on the returned sender resets
/// the timer; dropping it disarms the watchdog. Implemented with a plain
/// thread + `libc::shutdown` because this binary is built without tokio's
/// `time` feature.
fn spawn_idle_watchdog(fd: i32) -> std::sync::mpsc::Sender<()> {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let _ = std::thread::Builder::new().name("idle-watchdog".into()).spawn(move || {
        loop {
            match rx.recv_timeout(IDLE_TIMEOUT) {
                Ok(()) => continue, // activity: reset the timer
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    wlog(format!("[fd {}] idle timeout, closing connection", fd));
                    // Interrupts any pending async read on this socket.
                    unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
                    return;
                }
            }
        }
    });
    tx
}

async fn handle_connection(stream: tokio::net::TcpStream, addr: std::net::SocketAddr) {
    use std::os::fd::AsRawFd;
    let watchdog = spawn_idle_watchdog(stream.as_raw_fd());
    let (mut reader, mut writer) = stream.into_split();
    let mut buf = Vec::with_capacity(64 * 1024);
    wlog(format!("[{:?}] open", addr));

    loop {
        while buf.len() < 5 {
            match reader.read_buf(&mut buf).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) => { wlog(format!("[{:?}] read err: {}", addr, e)); return; }
            }
        }
        let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
        if len > MAX_REQUEST { wlog(format!("[{:?}] too big {}", addr, len)); return; }
        while buf.len() < 5 + len {
            match reader.read_buf(&mut buf).await {
                Ok(0) => { wlog(format!("[{:?}] premature eof", addr)); return; }
                Ok(_) => {}
                Err(e) => { wlog(format!("[{:?}] read err2: {}", addr, e)); return; }
            }
        }
        let op = buf[0];
        let args = buf[5..5+len].to_vec();
        buf.drain(..5+len);
        wlog(format!("[{:?}] op={:#x} len={}", addr, op, len));

        // The dispatch handlers use blocking libc calls (pread/pwrite/
        // readdir/stat/...); run them on the blocking pool so they cannot
        // stall the async runtime.
        let response = match tokio::task::spawn_blocking(move || dispatch(op, &args)).await {
            Ok(response) => response,
            Err(e) => {
                wlog(format!("[{:?}] dispatch task failed: {}", addr, e));
                return;
            }
        };
        wlog(format!("[{:?}] -> {} bytes", addr, response.len()));
        if writer.write_all(&response).await.is_err() { wlog(format!("[{:?}] write err", addr)); return; }
        if writer.flush().await.is_err() { return; }

        // Completed request: reset the idle timer.
        let _ = watchdog.send(());
    }
}

fn dispatch(op: u8, args: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    match op {
        0x01 => match handle_open(args) {
            Ok(fd) => { out.push(0u8); out.extend_from_slice(&fd.to_le_bytes()); }
            Err((s, msg)) => { out.push(s as u8); out.extend_from_slice(msg.as_bytes()); }
        },
        0x02 => {
            if args.len() < 4 { out.push(0x08); out.extend_from_slice(b"short"); }
            else {
                let fd = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
                let r = unsafe { libc::close(fd as i32) };
                if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"close failed"); }
            }
        }
        0x03 => {
            if args.len() < 16 { out.push(0x08); out.extend_from_slice(b"short"); }
            else {
                let fd = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
                let off = u64::from_le_bytes(args[4..12].try_into().unwrap());
                let len = u32::from_le_bytes([args[12], args[13], args[14], args[15]]) as usize;
                if len > MAX_RESPONSE { out.push(0x07); out.extend_from_slice(b"too big"); }
                else {
                    let mut tmp = vec![0u8; len];
                    let n = unsafe { libc::pread(fd as i32, tmp.as_mut_ptr() as *mut _, len, off as i64) };
                    if n < 0 { out.push(0x07); out.extend_from_slice(b"pread"); }
                    else {
                        tmp.truncate(n as usize);
                        out.push(0);
                        out.extend_from_slice(&tmp);
                    }
                }
            }
        }
        0x04 => {
            if args.len() < 16 { out.push(0x08); out.extend_from_slice(b"short"); }
            else {
                let fd = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
                let off = u64::from_le_bytes(args[4..12].try_into().unwrap());
                let len = u32::from_le_bytes([args[12], args[13], args[14], args[15]]) as usize;
                if args.len() < 16 + len { out.push(0x08); out.extend_from_slice(b"short"); }
                else {
                    let data = &args[16..16+len];
                    let n = unsafe { libc::pwrite(fd as i32, data.as_ptr() as *const _, len, off as i64) };
                    if n < 0 { out.push(0x07); out.extend_from_slice(b"pwrite"); }
                    else if (n as usize) != len { out.push(0x07); out.extend_from_slice(b"short write"); }
                    else { out.push(0); }
                }
            }
        }
        0x05 | 0x0F => match read_path(args) {
            Some((path, _)) => {
                let st = if op == 0x05 { do_stat(&path) } else { do_lstat(&path) };
                match st {
                    Ok(s) => { out.push(0); out.extend_from_slice(&s); }
                    Err((s, msg)) => { out.push(s as u8); out.extend_from_slice(msg.as_bytes()); }
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x06 => match read_path(args) {
            Some((path, _)) => match do_listdir(&path) {
                Ok(data) => { out.push(0); out.extend_from_slice(&data); }
                Err((s, msg)) => { out.push(s as u8); out.extend_from_slice(msg.as_bytes()); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x07 => match read_path(args) {
            Some((path, rest)) => {
                if rest.len() < 4 { out.push(0x08); out.extend_from_slice(b"short"); }
                else {
                    let mode = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
                    match cstring(&path) {
                        Some(p) => {
                            let r = unsafe { libc::mkdir(p.as_ptr(), mode) };
                            if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"mkdir"); }
                        }
                        None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
                    }
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x08 => match read_path(args) {
            Some((path, _)) => match cstring(&path) {
                Some(p) => {
                    let r = unsafe { libc::unlink(p.as_ptr()) };
                    if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"unlink"); }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x09 => match read_path(args) {
            Some((path, _)) => match cstring(&path) {
                Some(p) => {
                    let r = unsafe { libc::rmdir(p.as_ptr()) };
                    if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"rmdir"); }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0A => match read_path(args) {
            Some((src, rest)) => match read_path(rest) {
                Some((dst, _)) => match (cstring(&src), cstring(&dst)) {
                    (Some(s), Some(d)) => {
                        let r = unsafe { libc::rename(s.as_ptr(), d.as_ptr()) };
                        if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"rename"); }
                    }
                    _ => { out.push(0x08); out.extend_from_slice(b"bad path"); }
                },
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0B => match read_path(args) {
            Some((path, rest)) => {
                if rest.len() < 8 { out.push(0x08); out.extend_from_slice(b"short"); }
                else {
                    let size = u64::from_le_bytes(rest[0..8].try_into().unwrap());
                    match cstring(&path) {
                        Some(p) => {
                            let r = unsafe { libc::truncate(p.as_ptr(), size as i64) };
                            if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"truncate"); }
                        }
                        None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
                    }
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0C => match read_path(args) {
            Some((path, _)) => match cstring(&path) {
                Some(p) => {
                    let mut buf = vec![0u8; 4096];
                    let r = unsafe { libc::realpath(p.as_ptr(), buf.as_mut_ptr() as *mut _) };
                    if r.is_null() { out.push(0x01); out.extend_from_slice(b"realpath"); }
                    else {
                        let len = unsafe { libc::strlen(r) };
                        out.push(0);
                        out.extend_from_slice(&(len as u32).to_le_bytes());
                        out.extend_from_slice(unsafe { std::slice::from_raw_parts(r as *const u8, len) });
                    }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0D => match read_path(args) {
            Some((path, _)) => match cstring(&path) {
                Some(p) => {
                    let mut buf = vec![0u8; 4096];
                    let n = unsafe { libc::readlink(p.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
                    if n < 0 { out.push(0x07); out.extend_from_slice(b"readlink"); }
                    else {
                        out.push(0);
                        out.extend_from_slice(&(n as u32).to_le_bytes());
                        out.extend_from_slice(&buf[..n as usize]);
                    }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0E => match read_path(args) {
            Some((target, rest)) => match read_path(rest) {
                Some((linkpath, _)) => match (cstring(&target), cstring(&linkpath)) {
                    (Some(t), Some(l)) => {
                        let r = unsafe { libc::symlink(t.as_ptr(), l.as_ptr()) };
                        if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"symlink"); }
                    }
                    _ => { out.push(0x08); out.extend_from_slice(b"bad path"); }
                },
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x10 => match read_path(args) {
            Some((path, rest)) => {
                // Payload: [path][atime i64 LE][mtime i64 LE] — the client's
                // requested times (see adb_proxy::ProxyClient::utime). Use
                // them instead of unconditionally setting "now".
                if rest.len() < 16 { out.push(0x08); out.extend_from_slice(b"short"); }
                else {
                    let atime = i64::from_le_bytes(rest[0..8].try_into().unwrap());
                    let mtime = i64::from_le_bytes(rest[8..16].try_into().unwrap());
                    let times = [
                        libc::timeval { tv_sec: atime as libc::time_t, tv_usec: 0 },
                        libc::timeval { tv_sec: mtime as libc::time_t, tv_usec: 0 },
                    ];
                    match cstring(&path) {
                        Some(p) => {
                            let r = unsafe { libc::utimes(p.as_ptr(), times.as_ptr()) };
                            if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"utimes"); }
                        }
                        None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
                    }
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x11 => match read_path(args) {
            Some((path, _)) => match cstring(&path) {
                Some(p) => {
                    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
                    let r = unsafe { libc::statvfs(p.as_ptr(), &mut st) };
                    if r != 0 { out.push(0x07); out.extend_from_slice(b"statvfs"); }
                    else {
                        out.push(0);
                        out.extend_from_slice(&st.f_bavail.to_le_bytes());
                        out.extend_from_slice(&st.f_blocks.to_le_bytes());
                        out.extend_from_slice(&st.f_bsize.to_le_bytes());
                    }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x12 => match read_two_paths(args) {
            Some((src, dst)) => match do_copy_file(&src, &dst) {
                Ok(()) => { out.push(0); }
                Err((s, msg)) => { out.push(s); out.extend_from_slice(msg.as_bytes()); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        _ => { out.push(0x0B); out.extend_from_slice(b"unknown op"); }
    }
    let mut framed = Vec::with_capacity(out.len() + 4);
    framed.extend_from_slice(&(out.len() as u32).to_le_bytes());
    framed.extend_from_slice(&out);
    framed
}

fn read_two_paths(args: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (src, rest) = read_path(args)?;
    let (dst, rest) = read_path(rest)?;
    if !rest.is_empty() || [&src, &dst].iter().any(|p| !p.starts_with(b"/") || p.contains(&0)) {
        return None;
    }
    Some((src, dst))
}

fn copy_error(error: std::io::Error) -> (u8, String) {
    let status = match error.raw_os_error() {
        Some(libc::ENOENT) => 0x01,
        Some(libc::EACCES | libc::EPERM) => 0x02,
        Some(libc::EISDIR) => 0x03,
        Some(libc::ENOTDIR) => 0x04,
        Some(libc::EEXIST) => 0x05,
        Some(libc::EINVAL | libc::ELOOP) => 0x08,
        Some(libc::ENOSPC) => 0x09,
        Some(libc::ENAMETOOLONG) => 0x0A,
        _ => 0x07,
    };
    (status, error.to_string())
}

fn do_copy_file(src: &[u8], dst: &[u8]) -> Result<(), (u8, String)> {
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    use std::path::Path;

    if [src, dst].iter().any(|p| !p.starts_with(b"/") || p.contains(&0) || p.len() > MAX_PATH) {
        return Err((0x08, "bad path".into()));
    }
    let src = Path::new(std::ffi::OsStr::from_bytes(src));
    let source = OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(src).map_err(copy_error)?;
    let metadata = source.metadata().map_err(copy_error)?;
    if !metadata.is_file() {
        return Err((if metadata.is_dir() { 0x03 } else { 0x08 }, "source must be a regular file".into()));
    }
    if matches!(dst.rsplit(|b| *b == b'/').next(), Some(b"" | b"." | b"..")) {
        return Err((0x08, "destination must name a file".into()));
    }
    let dst = Path::new(std::ffi::OsStr::from_bytes(dst));
    let name = dst.file_name().ok_or_else(|| (0x08, "destination must name a file".into()))?;
    let parent = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY)
        .open(dst.parent().unwrap()).map_err(copy_error)?;
    let name = CString::new(name.as_bytes()).unwrap();
    let fd = unsafe {
        libc::openat(parent.as_raw_fd(), name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o666)
    };
    if fd < 0 { return Err(copy_error(std::io::Error::last_os_error())); }
    let mut destination = unsafe { File::from_raw_fd(fd) };
    copy_into_owned(source, metadata.len(), &mut destination, &parent, &name).map_err(copy_error)
}

fn copy_into_owned(
    source: impl std::io::Read,
    size: u64,
    destination: &mut std::fs::File,
    parent: &std::fs::File,
    name: &CString,
) -> std::io::Result<()> {
    let result = (|| {
        let copied = std::io::copy(&mut source.take(size), destination)?;
        if copied != size {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "source shortened during copy"));
        }
        destination.sync_all()
    })();
    if result.is_err() {
        remove_owned_partial(parent, name, destination);
    }
    result
}

fn remove_owned_partial(parent: &std::fs::File, name: &CString, file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let Ok(owned) = file.metadata() else { return };
    let mut current: libc::stat = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::fstatat(parent.as_raw_fd(), name.as_ptr(), &mut current, libc::AT_SYMLINK_NOFOLLOW)
    };
    if result == 0 && current.st_dev == owned.dev() && current.st_ino == owned.ino() {
        unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    }
}

fn read_path(args: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    if args.len() < 4 { return None; }
    let len = u32::from_le_bytes([args[0], args[1], args[2], args[3]]) as usize;
    if len > MAX_PATH || args.len() < 4 + len { return None; }
    Some((args[4..4+len].to_vec(), &args[4+len..]))
}

/// NUL-terminate a wire path for libc, rejecting embedded NUL bytes so a
/// hostile client cannot smuggle a shorter path past the declared length.
fn cstring(path: &[u8]) -> Option<CString> {
    CString::new(path.to_vec()).ok()
}

fn handle_open(args: &[u8]) -> std::result::Result<u32, (u8, &'static str)> {
    if args.len() < 8 { return Err((0x08, "short")); }
    let proto_flags = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
    let mode = u32::from_le_bytes([args[4], args[5], args[6], args[7]]);
    if args.len() < 12 { return Err((0x08, "short")); }
    let path_len = u32::from_le_bytes([args[8], args[9], args[10], args[11]]) as usize;
    if args.len() < 12 + path_len { return Err((0x08, "short")); }
    let path = &args[12..12+path_len];
    let mut cpath = path.to_vec();
    cpath.push(0);

    // Translate our wire-format flags (see `adb_proxy::OpenFlags`) to libc.
    const P_READ: u32 = 0x1;
    const P_WRITE: u32 = 0x2;
    const P_CREATE: u32 = 0x4;
    const P_EXCL: u32 = 0x8;
    const P_TRUNC: u32 = 0x10;
    const P_APPEND: u32 = 0x20;
    let mut lflags: i32 = 0;
    if proto_flags & P_READ != 0 && proto_flags & P_WRITE != 0 { lflags |= libc::O_RDWR; }
    else if proto_flags & P_WRITE != 0 { lflags |= libc::O_WRONLY; }
    else { lflags |= libc::O_RDONLY; }
    if proto_flags & P_CREATE != 0 { lflags |= libc::O_CREAT; }
    if proto_flags & P_EXCL != 0 { lflags |= libc::O_EXCL; }
    if proto_flags & P_TRUNC != 0 { lflags |= libc::O_TRUNC; }
    if proto_flags & P_APPEND != 0 { lflags |= libc::O_APPEND; }

    wlog(format!("open flags proto={:#x} libc={:#x} path={:?}", proto_flags, lflags, String::from_utf8_lossy(&cpath[..cpath.len()-1])));
    let fd = unsafe { libc::open(cpath.as_ptr() as *const _, lflags, mode) };
    if fd < 0 {
        let errno = std::io::Error::last_os_error();
        wlog(format!("  -> errno {} ({})", errno.raw_os_error().unwrap_or(-1), errno));
        Err((0x07, "open"))
    } else {
        wlog(format!("  -> fd {}", fd));
        Ok(fd as u32)
    }
}

fn do_stat(path: &[u8]) -> std::result::Result<Vec<u8>, (u8, &'static str)> {
    let mut cpath = path.to_vec();
    cpath.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::stat(cpath.as_ptr() as *const _, &mut st) };
    if r != 0 { return Err((0x01, "stat")); }
    Ok(encode_stat(&st))
}

fn do_lstat(path: &[u8]) -> std::result::Result<Vec<u8>, (u8, &'static str)> {
    let mut cpath = path.to_vec();
    cpath.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::lstat(cpath.as_ptr() as *const _, &mut st) };
    if r != 0 { return Err((0x01, "lstat")); }
    Ok(encode_stat(&st))
}

fn encode_stat(st: &libc::stat) -> Vec<u8> {
    let mut buf = Vec::with_capacity(60);
    buf.extend_from_slice(&(st.st_mode as u32).to_le_bytes());
    buf.extend_from_slice(&(st.st_size as u64).to_le_bytes());
    buf.extend_from_slice(&(st.st_mtime as i64).to_le_bytes());
    buf.extend_from_slice(&(st.st_atime as i64).to_le_bytes());
    buf.extend_from_slice(&(st.st_ctime as i64).to_le_bytes());
    buf.extend_from_slice(&(st.st_uid as u32).to_le_bytes());
    buf.extend_from_slice(&(st.st_gid as u32).to_le_bytes());
    buf.extend_from_slice(&(st.st_nlink as u32).to_le_bytes());
    buf.extend_from_slice(&(st.st_blksize as u32).to_le_bytes());
    buf.extend_from_slice(&(st.st_blocks as u64).to_le_bytes());
    buf
}

fn do_listdir(path: &[u8]) -> std::result::Result<Vec<u8>, (u8, &'static str)> {
    use std::ffi::CStr;
    let mut cpath = path.to_vec();
    cpath.push(0);
    let dir = unsafe { libc::opendir(cpath.as_ptr() as *const _) };
    if dir.is_null() { return Err((0x01, "opendir")); }
    let mut out = Vec::new();
    loop {
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() { break; }
        let name_ptr = unsafe { (*ent).d_name.as_ptr() };
        let name = unsafe { CStr::from_ptr(name_ptr) };
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." { continue; }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let mut full = path.to_vec();
        if !full.ends_with(b"/") { full.push(b'/'); }
        full.extend_from_slice(name_bytes);
        full.push(0);
        let r = unsafe { libc::lstat(full.as_ptr() as *const _, &mut st) };
        if r != 0 { continue; }
        out.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&encode_stat(&st));
    }
    unsafe { libc::closedir(dir) };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!("adbshare-copy-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> Vec<u8> {
            use std::os::unix::ffi::OsStrExt;
            self.0.join(name).as_os_str().as_bytes().to_vec()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn copy_args(src: &[u8], dst: &[u8]) -> Vec<u8> {
        let mut args = Vec::new();
        for path in [src, dst] {
            args.extend_from_slice(&(path.len() as u32).to_le_bytes());
            args.extend_from_slice(path);
        }
        args
    }

    #[test]
    fn copy_preserves_source_for_empty_small_and_large_files() {
        use std::os::unix::fs::MetadataExt;
        let dir = TestDir::new();
        for size in [0, 17, 8 * 1024 * 1024 + 19] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let src = dir.0.join("source");
            let dst = dir.0.join(format!("copy-{size}"));
            std::fs::write(&src, &data).unwrap();
            let before = std::fs::metadata(&src).unwrap();
            let response = dispatch(0x12, &copy_args(&dir.path("source"), &dir.path(&format!("copy-{size}"))));
            assert_eq!(response, [1, 0, 0, 0, 0]);
            assert_eq!(std::fs::read(&src).unwrap(), data);
            assert_eq!(std::fs::read(&dst).unwrap(), data);
            let after = std::fs::metadata(&src).unwrap();
            assert_eq!(before.ino(), after.ino());
            assert_eq!(before.mtime(), after.mtime());
            assert_eq!(before.mtime_nsec(), after.mtime_nsec());
            assert_ne!(after.ino(), std::fs::metadata(&dst).unwrap().ino());
        }
    }

    #[test]
    fn copy_refuses_existing_destinations_and_source_aliases() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::new();
        std::fs::write(dir.0.join("source"), b"source").unwrap();
        std::fs::write(dir.0.join("existing"), b"keep").unwrap();
        std::fs::hard_link(dir.0.join("source"), dir.0.join("hardlink")).unwrap();
        symlink("source", dir.0.join("symlink")).unwrap();
        symlink("missing", dir.0.join("dangling")).unwrap();
        std::fs::create_dir(dir.0.join("directory")).unwrap();
        for dst in ["source", "existing", "hardlink", "symlink", "dangling", "directory"] {
            let response = dispatch(0x12, &copy_args(&dir.path("source"), &dir.path(dst)));
            assert_eq!(response[4], 0x05, "{dst}: {response:?}");
            assert_eq!(std::fs::read(dir.0.join("source")).unwrap(), b"source");
        }
        assert_eq!(std::fs::read(dir.0.join("existing")).unwrap(), b"keep");
        assert_eq!(std::fs::read_link(dir.0.join("symlink")).unwrap(), std::path::Path::new("source"));
        assert_eq!(std::fs::read_link(dir.0.join("dangling")).unwrap(), std::path::Path::new("missing"));
        assert!(!dir.0.join("missing").exists());
        assert!(dir.0.join("directory").is_dir());
    }

    #[test]
    fn copy_refuses_nonregular_sources_without_creating_destination() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::new();
        std::fs::write(dir.0.join("source"), b"keep").unwrap();
        symlink("source", dir.0.join("symlink")).unwrap();
        symlink("missing", dir.0.join("dangling")).unwrap();
        std::fs::create_dir(dir.0.join("directory")).unwrap();
        let fifo = CString::new(dir.path("fifo")).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for src in ["symlink", "dangling", "directory", "fifo", "missing"] {
            assert!(do_copy_file(&dir.path(src), &dir.path("copy")).is_err(), "{src}");
            assert!(!dir.0.join("copy").exists());
        }
        assert_eq!(std::fs::read(dir.0.join("source")).unwrap(), b"keep");
    }

    #[test]
    fn copy_rejects_bad_paths_and_malformed_payloads() {
        let dir = TestDir::new();
        std::fs::write(dir.0.join("source"), b"keep").unwrap();
        for path in [b"".as_slice(), b"relative", b"/nul\0hidden", &vec![b'/'; MAX_PATH + 1]] {
            for args in [copy_args(path, &dir.path("copy")), copy_args(&dir.path("source"), path)] {
                assert_eq!(dispatch(0x12, &args)[4], 0x08);
            }
        }
        let args = copy_args(&dir.path("source"), &dir.path("copy"));
        for len in 0..args.len() {
            assert_eq!(dispatch(0x12, &args[..len])[4], 0x08);
        }
        let mut trailing = args;
        trailing.push(0);
        assert_eq!(dispatch(0x12, &trailing)[4], 0x08);
        for dst in ["copy/", "copy/.", "copy/.."] {
            assert!(do_copy_file(&dir.path("source"), &dir.path(dst)).is_err());
        }
        assert!(!dir.0.join("copy").exists());
    }

    #[test]
    fn copy_bounds_input_and_cleans_partial_on_read_error() {
        use std::io::{Cursor, Read};
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected read failure"))
            }
        }
        let dir = TestDir::new();
        let parent = std::fs::File::open(&dir.0).unwrap();
        let name = CString::new("partial").unwrap();
        for source in [
            Box::new(Cursor::new(b"short")) as Box<dyn Read>,
            Box::new(Read::chain(Cursor::new(b"partial"), FailingReader)),
        ] {
            let mut destination = std::fs::File::create(dir.0.join("partial")).unwrap();
            assert!(copy_into_owned(source, 100, &mut destination, &parent, &name).is_err());
            assert!(!dir.0.join("partial").exists());
        }
        let mut destination = std::fs::File::create(dir.0.join("partial")).unwrap();
        copy_into_owned(Cursor::new(b"initial-appended"), 7, &mut destination, &parent, &name).unwrap();
        assert_eq!(std::fs::read(dir.0.join("partial")).unwrap(), b"initial");
    }

    #[test]
    fn partial_cleanup_is_anchored_and_preserves_replacements() {
        use std::fs::File;
        use std::os::unix::fs::symlink;
        let dir = TestDir::new();
        let parent = File::open(&dir.0).unwrap();
        let name = CString::new("partial").unwrap();
        let partial = File::create(dir.0.join("partial")).unwrap();
        remove_owned_partial(&parent, &name, &partial);
        assert!(!dir.0.join("partial").exists());
        std::fs::write(dir.0.join("partial"), b"replacement").unwrap();
        remove_owned_partial(&parent, &name, &partial);
        assert_eq!(std::fs::read(dir.0.join("partial")).unwrap(), b"replacement");
        std::fs::remove_file(dir.0.join("partial")).unwrap();
        symlink("missing", dir.0.join("partial")).unwrap();
        remove_owned_partial(&parent, &name, &partial);
        assert!(std::fs::symlink_metadata(dir.0.join("partial")).unwrap().is_symlink());

        std::fs::create_dir(dir.0.join("parent")).unwrap();
        let parent = File::open(dir.0.join("parent")).unwrap();
        let partial = File::create(dir.0.join("parent/partial")).unwrap();
        std::fs::rename(dir.0.join("parent"), dir.0.join("moved")).unwrap();
        std::fs::create_dir(dir.0.join("parent")).unwrap();
        std::fs::write(dir.0.join("parent/partial"), b"keep").unwrap();
        remove_owned_partial(&parent, &name, &partial);
        assert!(!dir.0.join("moved/partial").exists());
        assert_eq!(std::fs::read(dir.0.join("parent/partial")).unwrap(), b"keep");
    }

    #[test]
    fn cstring_rejects_embedded_nul() {
        assert!(cstring(b"/tmp/ok").is_some());
        assert!(cstring(b"/tmp/a\0/hidden").is_none());
    }

    #[test]
    fn dispatch_unlink_rejects_nul_path() {
        let mut args = 9u32.to_le_bytes().to_vec();
        args.extend_from_slice(b"/tmp/a\0/hidden");
        let out = dispatch(0x08, &args);
        assert_eq!(out[4], 0x08);
        assert_eq!(&out[5..], b"bad path");
    }
}
