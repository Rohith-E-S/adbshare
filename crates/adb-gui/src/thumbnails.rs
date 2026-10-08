//! Thumbnails, by the same two mechanisms GNOME Files uses.
//!
//! Nautilus implements no image decoding of its own. `libgnome-desktop`'s
//! `ThumbnailFactory` looks for a `.thumbnailer` keyfile in
//! `$XDG_DATA_DIRS/thumbnailers` whose `MimeType` list covers the file, then
//! *runs the program it names* — `evince-thumbnailer` for PDF,
//! `totem-video-thumbnailer` for video, `gsf-office-thumbnailer` for office
//! documents — and only falls back to in-process decoding (gdk-pixbuf) when
//! nothing matches or the program fails. Results are cached in the
//! freedesktop thumbnail repository, `$XDG_CACHE_HOME/thumbnails`, keyed by
//! the MD5 of the file's canonical URI, with the source URI and mtime stored
//! as PNG `tEXt` keys so a changed file invalidates.
//!
//! Doing the same thing gets every preview type adbshare would otherwise have
//! to write a decoder for, with no new dependencies: on a desktop with
//! poppler and ffmpeg installed, PDFs and videos thumbnail the same pixels
//! Nautilus draws. So this module is the spec's cache plus a runner for the
//! installed thumbnailers, with in-process `image` decoding as the fallback
//! for the formats that need no help (JPEG, PNG, GIF, WebP, BMP, TIFF) and
//! for phones with no FUSE mount, where the bytes never touch a local path.
//!
//! Byte source. Everything here reads a *local path*. A phone file is turned
//! into one by prefixing the device's FUSE mountpoint, which the browser
//! already tracks; without FUSE there is no path and no D-Bus read, so phone
//! previews are skipped and the caller keeps its static icon. That is the
//! ceiling: `ponytail:` phone previews need FUSE. `adb-daemon --no-fuse`
//! trades them away.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gpui::{Image, ImageFormat};

/// Which of the spec's two sizes to use. The grid is bigger than the list, so
/// it wants `Large`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Size {
    Normal = 128,
    Large = 256,
}

impl Size {
    fn dir(self) -> &'static str {
        match self {
            Size::Normal => "normal",
            Size::Large => "large",
        }
    }

    fn px(self) -> u32 {
        self as u32
    }
}

/// Program name and version, for the per-program failure cache. The spec
/// scopes negative results to a program so one broken decoder cannot make
/// another program's thumbs look permanently unavailable.
const FAIL_DIR: &str = "adbshare-1.0";

// ── Worker pool ──────────────────────────────────────────────────────────────

/// How many thumbnails may be generated at once. Generation is either an
/// in-process decode or a subprocess, and both are expensive enough that a
/// screenful of videos would otherwise start four hundred ffmpeg processes and
/// spend longer in the scheduler than in the codecs.
const WORKERS: usize = 4;

/// Stop asking for new work past this. A queue this long means the backlog is
/// growing faster than it drains, and the user has scrolled a long way; the
/// entries still queued are ones they will reach. Dropping the newest requests
/// keeps the ones nearest the viewport.
const MAX_QUEUE: usize = 256;

type Key = (PathBuf, Size);

struct Pool {
    queue: Mutex<VecDeque<Key>>,
    wake: std::sync::Condvar,
    /// Finished work, drained by the view on its own thread.
    done: Mutex<Vec<(Key, Option<Thumb>)>>,
}

fn pool() -> &'static Arc<Pool> {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool = Arc::new(Pool {
            queue: Mutex::new(VecDeque::new()),
            wake: std::sync::Condvar::new(),
            done: Mutex::new(Vec::new()),
        });
        for _ in 0..WORKERS {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || {
                loop {
                    let job = {
                        let mut queue = pool.queue.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(job) = queue.pop_front() {
                                break job;
                            }
                            queue = pool.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let thumb = cached(&job.0, job.1).or_else(|| generate(&job.0, job.1));
                    pool.done
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((job, thumb));
                }
            });
        }
        pool
    })
}

/// Queue a thumbnail for generation. Returns immediately; the view picks the
/// result up from [`take_finished`] on a later frame.
///
/// Safe to call from the UI thread on every frame: a path already queued is
/// ignored, so it only has to be called for entries with no thumbnail yet.
pub fn request(path: &Path, size: Size) {
    let mut queue = pool().queue.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() >= MAX_QUEUE {
        return;
    }
    let key = (path.to_path_buf(), size);
    // Cheap dedup: a linear scan, but only over a bounded queue.
    if queue.iter().any(|q| *q == key) {
        return;
    }
    queue.push_back(key);
    pool().wake.notify_one();
}

/// Take everything the workers have finished since the last call.
///
/// Called from the view's own thread, so this is where results cross back onto
/// the UI thread — no executor or channel plumbing, and nothing that could
/// re-enter an in-progress update.
pub fn take_finished() -> Vec<(Key, Option<Thumb>)> {
    let mut done = pool().done.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *done)
}

// ── Cache location ───────────────────────────────────────────────────────────

fn cache_root() -> PathBuf {
    // `dirs` is already a dependency; XDG_CACHE_HOME is what the spec names.
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("thumbnails")
}

fn cache_path(uri: &str, size: Size) -> PathBuf {
    cache_root()
        .join(size.dir())
        .join(format!("{}.png", uri_md5(uri)))
}

fn fail_path(uri: &str) -> PathBuf {
    cache_root()
        .join("fail")
        .join(FAIL_DIR)
        .join(format!("{}.png", uri_md5(uri)))
}

/// The spec's filename hash: MD5 of the canonical absolute URI.
fn uri_md5(uri: &str) -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(uri.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Canonical `file://` URI for a local path, percent-encoded per the spec so
/// that a name with a space or a `#` in it hashes to the same value Nautilus
/// computes. Only the unreserved set survives unescaped; everything else
/// becomes `%XX` over the UTF-8 bytes.
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut uri = String::with_capacity(text.len() + 8);
    uri.push_str("file://");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char);
            }
            _ => {
                let _ = write!(uri, "%{byte:02X}");
            }
        }
    }
    uri
}

// ── PNG tEXt injection ───────────────────────────────────────────────────────

/// Insert `keys` as PNG `tEXt` chunks, returning a new file.
///
/// The cache's correctness rests on those keys: a PNG whose `Thumb::MTime` no
/// longer matches the source is stale and gets regenerated, which is the only
/// thing stopping a renamed or edited file from showing the old picture.
/// `image`'s PNG encoder will not write arbitrary chunks, and the thumbnailers
/// that write their own output generally omit them, so they go in here. Chunks
/// are `[len][type][data][crc32]`, so splicing them in ahead of the first
/// `IDAT` leaves every existing chunk — and its CRC — untouched.
fn png_with_text(png: &[u8], keys: &[(&str, &str)]) -> Vec<u8> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() < SIGNATURE.len() || png[..8] != SIGNATURE {
        return png.to_vec();
    }

    // Find where the first IDAT starts; everything before it is header chunks
    // we can append to.
    let mut insert_at = png.len();
    let mut at = 8usize;
    while at + 8 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap_or([0; 4])) as usize;
        let end = at + 12 + len;
        if &png[at + 4..at + 8] == b"IDAT" || end > png.len() {
            insert_at = at;
            break;
        }
        at = end;
    }

    let mut out = Vec::with_capacity(png.len() + 256 * keys.len());
    out.extend_from_slice(&png[..insert_at]);
    for (keyword, value) in keys {
        // `tEXt` is Latin-1 with no embedded NULs; our values are percent-
        // encoded URIs, decimal mtimes and byte counts, so all ASCII already.
        if keyword.is_empty() || keyword.len() > 79 || value.contains('\0') {
            continue;
        }
        let mut data = Vec::with_capacity(keyword.len() + value.len() + 1);
        data.extend_from_slice(keyword.as_bytes());
        data.push(0);
        data.extend_from_slice(value.as_bytes());

        let mut crc_input = Vec::with_capacity(4 + data.len());
        crc_input.extend_from_slice(b"tEXt");
        crc_input.extend_from_slice(&data);

        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(b"tEXt");
        out.extend_from_slice(&data);
        out.extend_from_slice(&crc32fast::hash(&crc_input).to_be_bytes());
    }
    out.extend_from_slice(&png[insert_at..]);
    out
}

/// Read the `tEXt` values out of a PNG. Only the keys this module writes are
/// of interest, and only the `tEXt` (uncompressed) form.
fn png_text(png: &[u8], wanted: &str) -> Option<String> {
    let mut at = 8usize;
    while at + 8 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().ok()?) as usize;
        let kind = png.get(at + 4..at + 8)?;
        let data = png.get(at + 8..at + 8 + len)?;
        if kind == b"tEXt" {
            let nul = data.iter().position(|&b| b == 0)?;
            if &data[..nul] == wanted.as_bytes() {
                return String::from_utf8(data[nul + 1..].to_vec()).ok();
            }
        }
        at += 12 + len;
    }
    None
}

// ── Public API ───────────────────────────────────────────────────────────────

/// A decoded thumbnail ready for [`gpui::img`].
pub type Thumb = Arc<Image>;

/// Wrap PNG bytes for the UI. The cache is always PNG, whatever the source
/// format was, so the format is known here.
fn into_thumb(png: Vec<u8>) -> Option<Thumb> {
    (!png.is_empty()).then(|| Arc::new(Image::from_bytes(ImageFormat::Png, png)))
}

/// Cache lookup for `path`. A hit is a `stat` plus one small file read, so it
/// is cheap enough to call while drawing a row. `None` means "not cached yet,
/// or stale" — either way the caller draws its icon and asks for a generation.
pub fn cached(path: &Path, size: Size) -> Option<Thumb> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let uri = file_uri(path);
    let file = cache_path(&uri, size);
    let png = std::fs::read(&file).ok()?;

    // The spec requires verifying the stored URI (hash collisions) and the
    // mtime (the file changed). Skipping this check is how a stale thumbnail
    // ends up permanently showing the wrong picture.
    match png_text(&png, "Thumb::MTime") {
        Some(stored) if stored == mtime_secs(mtime) => {}
        _ => return None,
    }
    if png_text(&png, "Thumb::URI").as_deref() != Some(uri.as_str()) {
        return None;
    }
    into_thumb(png)
}

fn mtime_secs(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

/// Generate a thumbnail for `path`, cache it, and return it.
///
/// Blocking and spawns subprocesses, so it must not run on the UI thread. Use
/// [`cached`] on that thread and hand this one to a worker.
pub fn generate(path: &Path, size: Size) -> Option<Thumb> {
    let uri = file_uri(path);
    if fail_path(&uri).exists() {
        return None;
    }
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let bytes = std::fs::metadata(path).ok()?.len();

    let raw = run_thumbnailer(path, size).or_else(|| read_source(path));

    let Some(png) = raw.as_deref().and_then(|b| encode_thumb(b, size)) else {
        // Negative-cache it so a codec we cannot handle, or a document with no
        // renderer installed, is not retried on every repaint of every row.
        mark_failed(&uri);
        return None;
    };

    let png = png_with_text(
        &png,
        &[
            ("Thumb::URI", &uri),
            ("Thumb::MTime", &mtime_secs(mtime)),
            ("Thumb::Size", &bytes.to_string()),
        ],
    );
    let dest = cache_path(&uri, size);
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Best effort: a cache we could not write still yields this session's
    // thumbnail, it just gets regenerated next time.
    let _ = std::fs::write(&dest, &png);
    into_thumb(png)
}

fn mark_failed(uri: &str) {
    let path = fail_path(uri);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, b"");
}

// ── Extension → MIME type ────────────────────────────────────────────────────

/// MIME types that could name this extension.
///
/// The `.thumbnailer` files declare MIME types, so matching one needs the type,
/// and adbshare has an extension rather than a content type. Resolving that
/// properly means `shared-mime-info` and a second source of truth for the same
/// extensions, so this is a short list of the aliases each format is known by
/// instead — the installed thumbnailers disagree with each other (ffmpeg's
/// claims both `video/matroska` and `video/x-matroska`; GNOME's audio one lists
/// six spellings of MP3), and a single guess silently matches nothing.
///
/// The list is only ever consulted to ask "is there an installed thumbnailer for
/// this?", so an alias that misses just falls through to the in-process decoder.
pub fn mimes_for(ext: &str) -> &'static [&'static str] {
    match ext {
        "jpg" | "jpeg" | "jpe" => &["image/jpeg", "image/jpg"],
        "png" => &["image/png"],
        "apng" => &["image/apng"],
        "gif" => &["image/gif"],
        "webp" => &["image/webp"],
        "bmp" => &[
            "image/bmp",
            "image/x-bmp",
            "image/x-MS-bmp",
            "image/vnd.microsoft.icon",
        ],
        "tif" | "tiff" => &["image/tiff"],
        "tga" => &["image/x-tga"],
        "exr" => &["image/x-exr"],
        "qoi" => &["image/x-qoi", "image/qoi"],
        "ico" => &["image/vnd.microsoft.icon", "image/x-icon", "image/ico"],
        "pbm" | "pgm" | "ppm" | "pnm" => &[
            "image/x-portable-bitmap",
            "image/x-portable-graymap",
            "image/x-portable-pixmap",
            "image/x-portable-anymap",
        ],
        "svg" | "svgz" => &["image/svg+xml", "image/svg+xml-compressed"],
        "avif" => &["image/avif", "image/avif-sequence"],
        "jxl" => &["image/jxl"],
        "heic" | "heif" => &["image/heic", "image/heif", "image/heic-sequence"],
        "pdf" => &["application/pdf", "application/x-pdf"],
        "ps" | "eps" => &["application/postscript", "image/x-eps", "image/eps"],
        "dvi" => &["application/x-dvi", "application/x-evince-dvi"],
        "cbz" | "cb7" | "cbt" | "cbr" => &[
            "application/vnd.comicbook+zip",
            "application/vnd.comicbook-rar",
            "application/x-cbz",
            "application/x-cb7",
            "application/x-cbt",
            "application/x-cbr",
        ],
        "mp4" | "m4v" => &["video/mp4", "video/quicktime", "video/x-m4v"],
        "mkv" | "mka" => &[
            "video/x-matroska",
            "video/matroska",
            "application/matroska",
            "application/x-matroska",
        ],
        "webm" => &["video/webm", "audio/webm"],
        "avi" => &["video/vnd.avi", "video/avi", "video/x-msvideo"],
        "mov" => &["video/quicktime"],
        "mpg" | "mpeg" | "mpe" => &["video/mpeg", "video/x-mpeg", "video/vnd.mpegurl"],
        "wmv" => &["video/x-ms-wmv"],
        "flv" => &["video/x-flv"],
        "3gp" => &["video/3gpp"],
        "ogv" => &["video/ogg"],
        "m2ts" | "ts" => &["video/mp2t"],
        "mp3" => &[
            "audio/mpeg",
            "audio/mp3",
            "audio/x-mpeg",
            "audio/x-mp3",
            "audio/mpeg3",
            "audio/x-mpeg-3",
        ],
        "flac" => &["audio/flac", "application/x-flac", "audio/x-flac"],
        "ogg" | "oga" => &["audio/ogg", "audio/vorbis", "audio/x-vorbis+ogg"],
        "opus" => &["audio/opus"],
        "wav" => &["audio/wav", "audio/x-wav", "audio/vnd.wave"],
        "m4a" => &["audio/mp4", "audio/m4a", "audio/x-m4a", "video/mp4"],
        "aac" => &["audio/aac", "audio/x-aac"],
        "wma" => &["audio/x-ms-wma", "audio/ms-wma"],
        "mid" | "midi" => &["audio/midi", "audio/x-midi"],
        "aiff" | "aif" => &["audio/x-aiff", "audio/x-pn-aiff"],
        "ape" => &["audio/x-ape", "audio/x-musepack"],
        "wv" => &["audio/x-wavpack"],
        "doc" => &["application/msword"],
        "docx" => &["application/vnd.openxmlformats-officedocument.wordprocessingml.document"],
        "xls" => &["application/vnd.ms-excel"],
        "xlsx" => &["application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"],
        "ppt" => &["application/vnd.ms-powerpoint"],
        "pptx" => &["application/vnd.openxmlformats-officedocument.presentationml.presentation"],
        "odt" | "ott" => &["application/vnd.oasis.opendocument.text"],
        "ods" | "ots" => &["application/vnd.oasis.opendocument.spreadsheet"],
        "odp" | "otp" => &["application/vnd.oasis.opendocument.presentation"],
        "odg" | "otg" => &["application/vnd.oasis.opendocument.graphics"],
        "odc" => &["application/vnd.oasis.opendocument.chart"],
        "odf" => &["application/vnd.oasis.opendocument.formula"],
        "fodt" | "fods" | "fodp" => &["application/vnd.oasis.opendocument.text"],
        _ => &[],
    }
}

/// A thumbnailer program installed on the system.
struct Thumbnailer {
    program: String,
    exec: String,
    mimes: Vec<String>,
}

/// Scan the thumbnailer directories once. Later directories lose to earlier
/// ones, matching `libgnome-desktop`: `$XDG_DATA_HOME/thumbnailers` overrides
/// the system ones, which is the whole point of the `$i` override.
fn thumbnailers() -> &'static [Thumbnailer] {
    static LOADED: OnceLock<Vec<Thumbnailer>> = OnceLock::new();
    LOADED.get_or_init(|| {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(home) = dirs::data_dir() {
            dirs.push(home.join("thumbnailers"));
        }
        if let Ok(system) = std::env::var("XDG_DATA_DIRS") {
            dirs.extend(std::env::split_paths(&system).map(|d| d.join("thumbnailers")));
        } else {
            dirs.push(PathBuf::from("/usr/local/share/thumbnailers"));
            dirs.push(PathBuf::from("/usr/share/thumbnailers"));
        }

        let mut found = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("thumbnailer") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some((exec, mimes)) = parse_thumbnailer(&text) else {
                    continue;
                };
                let program = exec.split_whitespace().next().unwrap_or_default();
                if program.is_empty() {
                    continue;
                }
                // `TryExec` is an optional absolute path that gates the whole
                // entry. Honour it rather than probing the program.
                if let Some(try_exec) = keyfile_value(&text, "TryExec") {
                    let found = if Path::new(&try_exec).is_absolute() {
                        Path::new(&try_exec).is_file()
                    } else {
                        // GNOME writes bare names like `pdftoppm`; resolve
                        // against PATH the way execvp would.
                        std::env::var_os("PATH").is_some_and(|path| {
                            std::env::split_paths(&path).any(|dir| dir.join(&try_exec).is_file())
                        })
                    };
                    if !found {
                        continue;
                    }
                }
                found.push(Thumbnailer {
                    program: program.to_string(),
                    exec,
                    mimes,
                });
            }
        }
        found
    })
}

/// The `[Thumbnailer Entry]` `Exec` and `MimeType` lines, or `None` if either is
/// missing — the spec makes both required.
fn parse_thumbnailer(text: &str) -> Option<(String, Vec<String>)> {
    let exec = keyfile_value(text, "Exec")?;
    let mimes = keyfile_value(text, "MimeType")?
        .split(';')
        .map(|m| m.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>();
    (!mimes.is_empty()).then_some((exec, mimes))
}

/// One value from a keyfile, ignoring comments and section headers. Keys are
/// matched case-insensitively, as the GLib keyfile parser does.
fn keyfile_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Run the installed thumbnailer that claims `ext`, if there is one.
fn run_thumbnailer(path: &Path, size: Size) -> Option<Vec<u8>> {
    let candidates = mimes_for(path.extension()?.to_str()?);
    if candidates.is_empty() {
        return None;
    }
    let thumb = thumbnailers()
        .iter()
        .find(|t| t.mimes.iter().any(|m| candidates.contains(&m.as_str())))?;

    // `%o` gets a temp path rather than the cache path: the output needs the
    // `Thumb::` keys added and the size bound enforced, which means decoding
    // and re-encoding it anyway, and a thumbnailer that fails halfway must not
    // leave a plausible-looking file where the cache is read from.
    //
    // Unique per job, not per process. The pool runs four of these at once, and
    // a shared name means they overwrite each other's output and then delete it
    // out from under a peer mid-read. That showed up as every PDF and video
    // silently falling back to its icon while the same file thumbnailed fine on
    // its own, and then getting negative-cached so it never retried.
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let out = std::env::temp_dir().join(format!("adbshare-thumb-{}-{seq}.png", std::process::id()));
    let uri = file_uri(path);
    let input = path.to_string_lossy().into_owned();

    let mut command = std::process::Command::new(&thumb.program);
    for token in thumb.exec.split_whitespace().skip(1) {
        let arg = match token {
            "%s" => size.px().to_string(),
            "%u" => uri.clone(),
            "%i" => input.clone(),
            "%o" => out.to_string_lossy().into_owned(),
            "%%" => "%".to_string(),
            // An unknown substitution is passed through rather than dropped:
            // the spec says only the five above are defined, so a thumbnailer
            // using something else is already out of spec.
            other => other.to_string(),
        };
        command.arg(arg);
    }

    // ponytail: no timeout on the child. A wedged ffmpeg on a truncated video
    // holds this worker thread for as long as the codec spins. Add a kill
    // timer if a thumbnail ever hangs the pool.
    let status = command.output().ok()?;
    if !status.status.success() {
        let _ = std::fs::remove_file(&out);
        return None;
    }
    let produced = std::fs::read(&out).ok();
    let _ = std::fs::remove_file(&out);
    produced
}

/// Scale so the longest side is at most `size`, as the spec requires, and
/// re-encode as PNG. Not a square canvas: the spec's own thumbnails are
/// whatever aspect the source was (GNOME writes 256x144 for a 16:9 video), and
/// the view letterboxes them.
fn fit_within_box(img: image::DynamicImage, size: Size) -> image::DynamicImage {
    let bound = size.px();
    let (w, h) = (img.width().max(1), img.height().max(1));
    if w <= bound && h <= bound {
        return img;
    }
    img.resize(bound, bound, image::imageops::FilterType::Lanczos3)
}

/// Normalise whatever bytes we have into a bounded PNG.
///
/// This runs on a thumbnailer's output too, not just on the in-process decode,
/// and it has to: the cache is PNG and carries `Thumb::` keys that only a PNG
/// can hold, but nothing forces a thumbnailer to *write* a PNG. The video one
/// this was developed against does not pass `-c png`, so ffmpeg hands back a
/// JPEG under a `.png` cache filename, which then never validates and is
/// regenerated on every single visit. Decoding and re-encoding is what
/// libgnome-desktop does for the same reason.
fn encode_thumb(raw: &[u8], size: Size) -> Option<Vec<u8>> {
    let decoded = image::load_from_memory(raw).ok()?;
    let scaled = fit_within_box(decoded, size);
    let mut out = Vec::new();
    scaled
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    (!out.is_empty()).then_some(out)
}

/// Read the source file itself. The in-process fallback for the formats `image`
/// decodes on its own.
///
/// SVG is deliberately not handled here: rasterising one needs a layout engine
/// `image` does not have, and gpui's SVG path keeps only the alpha channel —
/// the opposite of what a preview needs. SVG relies on the installed
/// thumbnailer (`rsvg-convert`, `glycin-svg`) and falls through when there is
/// none.
fn read_source(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache identity and the chunk splicing both break silently: a wrong
    /// key means every thumbnail on the desktop is regenerated and the bug
    /// reads as "previews are just slow".
    #[test]
    fn the_cache_key_matches_a_real_gnome_thumbnail() {
        // The expected value is the filename of a thumbnail GNOME's own
        // ThumbnailFactory wrote, taken from ~/.cache/thumbnails/large on a
        // GNOME desktop. If our key disagreed with it, adbshare would ignore
        // every thumbnail Nautilus had already made.
        assert_eq!(
            uri_md5(
                "file:///home/rohith/Downloads/MotoGP.2014.Round10.USA.Indianapolis/\
                 MotoGP.2014.Round10.USA.Indianapolis.Qualifying.Sat.Feed.720p.x264.Multi.Language.mkv"
            ),
            "00560604efe2f70ff9217d5425300f3f"
        );
        // The key has to be over the URI, so two names in one directory differ.
        assert_ne!(uri_md5("file:///tmp/a.jpg"), uri_md5("file:///tmp/b.jpg"));
    }

    #[test]
    fn uris_are_percent_encoded_but_keep_separators() {
        assert_eq!(file_uri(Path::new("/tmp/a b.jpg")), "file:///tmp/a%20b.jpg");
        assert_eq!(file_uri(Path::new("/tmp/a#b.jpg")), "file:///tmp/a%23b.jpg");
        // A literal `%` has to be escaped or the URI is ambiguous.
        assert_eq!(
            file_uri(Path::new("/tmp/100%.jpg")),
            "file:///tmp/100%25.jpg"
        );
        assert_eq!(file_uri(Path::new("/tmp/a+b.jpg")), "file:///tmp/a%2Bb.jpg");
        // Path separators stay literal: escaping them would change the path.
        assert_eq!(
            file_uri(Path::new("/sdcard/DCIM/x.png")),
            "file:///sdcard/DCIM/x.png"
        );
    }

    #[test]
    fn text_chunks_survive_a_png_round_trip() {
        let mut encoded = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([1, 2, 3, 255]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::Png,
        )
        .unwrap();

        let tagged = png_with_text(
            &encoded,
            &[
                ("Thumb::URI", "file:///tmp/a.jpg"),
                ("Thumb::MTime", "1700000000"),
            ],
        );
        assert_eq!(
            png_text(&tagged, "Thumb::URI").as_deref(),
            Some("file:///tmp/a.jpg")
        );
        assert_eq!(
            png_text(&tagged, "Thumb::MTime").as_deref(),
            Some("1700000000")
        );
        // The pixels must still be readable, or the keys bought nothing.
        assert!(image::load_from_memory(&tagged).is_ok());
        // And an unkeyed PNG must read as having no keys, so a stale entry is
        // rejected rather than accepted on a missing match.
        assert_eq!(png_text(&encoded, "Thumb::MTime"), None);
    }

    #[test]
    fn non_png_input_is_passed_through_untouched() {
        assert_eq!(
            png_with_text(b"not a png at all", &[("K", "V")]),
            b"not a png at all"
        );
    }

    #[test]
    fn keyfiles_ignore_comments_and_sections() {
        let text = "[Thumbnailer Entry]\n\
                    # a comment\n\
                    Exec=pdftoppm %i %o\n\
                    MimeType=application/pdf; application/x-gzpdf\n";
        let (exec, mimes) = parse_thumbnailer(text).unwrap();
        assert_eq!(exec, "pdftoppm %i %o");
        assert_eq!(mimes, ["application/pdf", "application/x-gzpdf"]);
        // Exec is required as much as MimeType.
        assert!(parse_thumbnailer("[Thumbnailer Entry]\nMimeType=application/pdf\n").is_none());
        assert!(parse_thumbnailer("[Thumbnailer Entry]\nExec=x\n").is_none());
    }

    #[test]
    fn the_types_a_phone_is_full_of_are_covered() {
        for ext in [
            "jpg", "png", "svg", "gif", "webp", "pdf", "mp4", "mkv", "webm", "mov", "avi", "mp3",
            "flac", "m4a", "wav", "docx", "odt", "heic", "jxl", "avif",
        ] {
            assert!(!mimes_for(ext).is_empty(), "{ext} has no MIME type");
        }
        // Anything unrecognised must fall through to the static icon rather
        // than crash or be handed to a thumbnailer that cannot read it.
        assert!(mimes_for("xyz").is_empty());
        assert!(mimes_for("").is_empty());
    }

    /// Every entry adbshare claims to thumbnail has to be claimed by at least
    /// one thumbnailer installed on this machine, or the preview silently never
    /// appears and the icon just stays. `mimes_for` guessing a name the
    /// installed files do not use is exactly how that happens, so this asserts
    /// against the real directory rather than a comment.
    #[test]
    fn this_machine_has_a_thumbnailer_for_what_we_claim() {
        let known = thumbnailers();
        if known.is_empty() {
            // Nothing installed; the in-process decoder is the whole story and
            // there is nothing to check.
            return;
        }
        // The formats a phone camera produces, plus the documents and archives
        // people actually browse.
        for ext in [
            "jpg", "png", "gif", "webp", "heic", "pdf", "mp4", "mkv", "webm", "mov", "mp3", "m4a",
            "docx", "odt",
        ] {
            let candidates = mimes_for(ext);
            let matched = known
                .iter()
                .any(|t| t.mimes.iter().any(|m| candidates.contains(&m.as_str())));
            assert!(
                matched,
                "no installed thumbnailer claims {ext} ({candidates:?})"
            );
        }
    }
}
