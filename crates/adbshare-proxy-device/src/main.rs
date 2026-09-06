use std::env;
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
        wlog(format!("[main] waiting for connection"));
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
                    let r = unsafe { libc::mkdir(path.as_ptr() as *const _, mode) };
                    if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"mkdir"); }
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x08 => match read_path(args) {
            Some((path, _)) => {
                let r = unsafe { libc::unlink(path.as_ptr() as *const _) };
                if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"unlink"); }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x09 => match read_path(args) {
            Some((path, _)) => {
                let r = unsafe { libc::rmdir(path.as_ptr() as *const _) };
                if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"rmdir"); }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0A => match read_path(args) {
            Some((src, rest)) => match read_path(&rest) {
                Some((dst, _)) => {
                    let r = unsafe { libc::rename(src.as_ptr() as *const _, dst.as_ptr() as *const _) };
                    if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"rename"); }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0B => match read_path(args) {
            Some((path, rest)) => {
                if rest.len() < 8 { out.push(0x08); out.extend_from_slice(b"short"); }
                else {
                    let size = u64::from_le_bytes(rest[0..8].try_into().unwrap());
                    let r = unsafe { libc::truncate(path.as_ptr() as *const _, size as i64) };
                    if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"truncate"); }
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0C => match read_path(args) {
            Some((path, _)) => {
                let mut buf = vec![0u8; 4096];
                let r = unsafe { libc::realpath(path.as_ptr() as *const _, buf.as_mut_ptr() as *mut _) };
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
        0x0D => match read_path(args) {
            Some((path, _)) => {
                let mut buf = vec![0u8; 4096];
                let n = unsafe { libc::readlink(path.as_ptr() as *const _, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n < 0 { out.push(0x07); out.extend_from_slice(b"readlink"); }
                else {
                    out.push(0);
                    out.extend_from_slice(&(n as u32).to_le_bytes());
                    out.extend_from_slice(&buf[..n as usize]);
                }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x0E => match read_path(args) {
            Some((target, rest)) => match read_path(&rest) {
                Some((linkpath, _)) => {
                    let r = unsafe { libc::symlink(target.as_ptr() as *const _, linkpath.as_ptr() as *const _) };
                    if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"symlink"); }
                }
                None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
            },
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x10 => match read_path(args) {
            Some((path, _)) => {
                let r = unsafe { libc::utimes(path.as_ptr() as *const _, std::ptr::null()) };
                if r == 0 { out.push(0); } else { out.push(0x07); out.extend_from_slice(b"utimes"); }
            }
            None => { out.push(0x08); out.extend_from_slice(b"bad path"); }
        },
        0x11 => match read_path(args) {
            Some((path, _)) => {
                let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
                let r = unsafe { libc::statvfs(path.as_ptr() as *const _, &mut st) };
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
        _ => { out.push(0x0B); out.extend_from_slice(b"unknown op"); }
    }
    let mut framed = Vec::with_capacity(out.len() + 4);
    framed.extend_from_slice(&(out.len() as u32).to_le_bytes());
    framed.extend_from_slice(&out);
    framed
}

fn read_path(args: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    if args.len() < 4 { return None; }
    let len = u32::from_le_bytes([args[0], args[1], args[2], args[3]]) as usize;
    if len > MAX_PATH || args.len() < 4 + len { return None; }
    Some((args[4..4+len].to_vec(), &args[4+len..]))
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
