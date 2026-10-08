//! File-type icons read from the desktop's installed icon themes.
//!
//! The eight vendored artworks in `assets/art/` cover eight families, and
//! everything else fell through to one blue page — so a spreadsheet, a
//! Markdown file, a web page, a disc image and an executable all looked the
//! same, and none of them matched what the file manager beside them showed.
//!
//! GNOME Files does not own those icons either. It asks the freedesktop icon
//! theme, which every desktop installs, and those themes ship full-colour
//! `mimetypes/*.png` at every size: 981 of them across Yaru, Adwaita and
//! hicolor on this machine. Reading them gives complete coverage and exactly
//! the icons the rest of the desktop draws, with no assets vendored into the
//! binary and no new artwork to maintain.
//!
//! Only PNG is read. The theme's `scalable/` copies are SVG, and GPUI's SVG
//! path keeps just the alpha channel and tints the silhouette — which would
//! flatten a coloured spreadsheet logo into a green blob. `img()` keeps pixels,
//! so the raster `mimetypes/` directory is the right source and every theme
//! has it.
//!
//! Vendored art still wins where it exists, so the app keeps its own look for
//! the families it was designed around; this is the layer that answers for
//! everything else.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use gpui::prelude::*;
use gpui::{AnyElement, Image, ImageFormat};

/// Icon names, keyed by extension.
///
/// The names are the freedesktop convention, so they resolve on any desktop
/// that has a theme installed, rather than being adbshare's own vocabulary.
/// Grouped by family because that is how the desktop groups them too.
///
/// This is a fallback table, not the whole story: it is only consulted for a
/// file with no preview and no vendored artwork, and the names it names are
/// verified against the installed themes by a test — a typo here would
/// otherwise be a silent blank square.
pub fn name_for(ext: &str) -> Option<&'static str> {
    Some(match ext {
        // ── Documents ──────────────────────────────────────────────────
        "pdf" => "application-pdf",
        "ps" | "eps" | "epsi" | "psd" | "ai" => "application-postscript",
        "djvu" | "djv" => "x-office-document",
        "epub" | "mobi" | "azw3" | "fb2" => "application-epub+zip",
        "rtf" => "application-rtf",
        "tex" | "latex" | "sty" | "cls" | "bib" => "text-x-tex",

        // ── Office ─────────────────────────────────────────────────────
        "odt" | "ott" | "fodt" | "sxw" | "stw" => "application-vnd.oasis.opendocument.text",
        "ods" | "ots" | "fods" | "sxc" | "stc" | "csv" | "tsv" => {
            "application-vnd.oasis.opendocument.spreadsheet"
        }
        "odp" | "otp" | "fodp" | "sxi" | "sti" => "application-vnd.oasis.opendocument.presentation",
        "odg" | "otg" | "odg~" | "sxd" => "application-vnd.oasis.opendocument.graphics",
        "odc" => "application-vnd.oasis.opendocument.chart",
        "odb" => "application-vnd.oasis.opendocument.database",
        "doc" | "dot" => "application-msword",
        "docx" | "docm" | "dotx" => {
            "application-vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        "xls" | "xlt" => "application-vnd.ms-excel",
        "xlsx" | "xlsm" | "xltx" => {
            "application-vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        }
        "ppt" | "pot" | "pps" => "application-vnd.ms-powerpoint",
        "pptx" | "pptm" | "ppsx" => {
            "application-vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        "vsd" | "vsdx" => "x-office-document",
        "pub" => "application-vnd.ms-publisher",
        "mdb" | "accdb" => "application-vnd.ms-access",
        "one" => "x-office-document",

        // ── Text and markup ────────────────────────────────────────────
        "txt" | "text" | "nfo" | "readme" => "text-x-generic",
        "md" | "markdown" | "mkd" | "mdx" => "text-markdown",
        "rst" => "text-x-generic",
        "log" => "text-x-log",
        "html" | "htm" | "xhtml" => "text-html",
        "css" => "text-css",
        "scss" | "sass" => "text-x-scss",
        "less" => "text-less",
        "xml" | "xsd" | "xsl" | "svgz" | "rss" | "atom" => "text-xml",
        "json" | "json5" | "jsonc" => "application-json",
        "toml" => "application-toml",
        "yaml" | "yml" => "application-yaml",
        "sql" | "db" | "sqlite" | "sqlite3" | "mdb3" => "application-sql",
        "patch" | "diff" => "text-x-patch",

        // ── Code ───────────────────────────────────────────────────────
        "py" | "pyw" | "pyi" => "text-x-python",
        "rs" => "text-rust",
        "js" | "mjs" | "cjs" => "text-x-javascript",
        "ts" | "tsx" => "text-x-typescript",
        "jsx" => "text-x-javascript",
        "c" | "h" => "text-x-csrc",
        "cpp" | "cxx" | "cc" | "hpp" | "hxx" => "text-x-c++src",
        "cs" => "text-x-csharp",
        "java" => "text-x-java",
        "kt" | "kts" => "text-x-kotlin",
        "go" => "text-x-go",
        "rb" | "erb" | "gemspec" => "text-x-ruby",
        "php" => "text-x-php",
        "pl" | "pm" => "application-x-perl",
        "lua" => "text-x-lua",
        "sh" | "bash" | "zsh" | "ksh" | "fish" => "text-x-script",
        "ps1" => "text-x-generic",
        "swift" => "text-x-generic",
        "hs" | "lhs" => "text-x-haskell",
        "r" | "R" => "text-x-r-source",
        "scala" | "sc" => "text-x-scala",
        "dart" => "text-x-script",
        "nim" | "nims" => "text-x-nim",
        "v" => "text-x-v",
        "ex" | "exs" => "text-x-generic",
        "erl" | "hrl" => "text-x-generic",
        "clj" | "cljs" => "text-x-script",
        "lisp" | "el" => "text-x-script",
        "ml" | "mli" => "text-x-script",
        "pas" | "pp" => "text-x-script",
        "f" | "f90" | "f95" | "f03" => "text-x-fortran",
        "cob" | "cbl" => "text-x-cobol",
        "asm" | "s" => "text-x-script",
        "make" | "mk" | "mak" | "cmake" => "text-x-makefile",
        "gyp" | "gypi" => "text-x-generic",
        "gradle" => "text-x-generic",
        "dockerfile" => "text-dockerfile",
        "service" | "socket" | "timer" | "mount" | "target" => "text-x-systemd-unit",
        "desktop" => "application-x-desktop",
        "rc" | "conf" | "cfg" | "ini" | "env" | "properties" | "editorconfig" => "text-x-generic",
        "gpx" | "kml" | "kmz" | "geojson" => "application-gpx+xml",

        // ── Archives and packages ──────────────────────────────────────
        "zip" | "zipx" => "application-zip",
        "tar" => "application-x-archive",
        "gz" | "tgz" => "application-gzip",
        "bz2" | "tbz" | "tbz2" => "application-x-archive",
        "xz" | "txz" => "application-x-archive",
        "zst" | "tzst" => "application-x-archive",
        "lz" | "lz4" | "lzma" => "application-x-archive",
        "7z" => "application-x-7z-compressed",
        "rar" | "cbr" => "application-rar",
        "z" => "application-x-compress",
        "iso" | "img" | "dmg" | "vhd" | "vhdx" | "qcow" | "qcow2" | "vmdk" => "media-optical",
        "deb" => "application-vnd.debian.binary-package",
        "rpm" => "application-x-archive",
        "apk" => "application-apk",
        "snap" => "application-vnd.snap",
        "flatpak" | "flatpakref" => "application-vnd.flatpak",
        "appimage" => "application-x-executable",
        "exe" | "msi" | "com" | "bat" | "cmd" | "scr" => {
            "application-vnd.microsoft.portable-executable"
        }
        "app" => "application-x-executable",
        "jar" | "war" | "ear" => "application-x-java-archive",
        "whl" => "text-x-python",
        "gem" | "crate" | "nupkg" => "package-x-generic",
        "squashfs" | "squash" => "application-vnd.squashfs",

        // ── Media ──────────────────────────────────────────────────────
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff"
        | "ico" | "icns" | "heic" | "heif" | "avif" | "jxl" | "qoi" | "exr" | "tga" | "ppm"
        | "pgm" | "pbm" | "xcf" | "svg" => "image-x-generic",
        "mp3" | "flac" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "wma" | "aiff"
        | "aif" | "mid" | "midi" | "ape" | "wv" | "amr" | "mka" => "audio-x-generic",
        "mp4" | "mkv" | "avi" | "webm" | "mov" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" | "mpe"
        | "3gp" | "ogv" | "m2ts" | "vob" | "asf" | "rmvb" | "divx" => "video-x-generic",

        // ── Fonts ──────────────────────────────────────────────────────
        "ttf" | "otf" | "woff" | "woff2" | "eot" | "pfb" | "pfa" | "ttc" | "otc" => {
            "font-x-generic"
        }

        // ── Keys, certificates, contacts ───────────────────────────────
        "pem" | "crt" | "cer" | "der" | "p7b" | "p7c" | "pfx" | "p12" | "gpg" | "asc" | "key"
        | "ppk" => "application-pgp-keys",
        "sig" => "application-pgp-signature",
        "enc" => "application-pgp-encrypted",
        "vcf" | "vcard" | "vcs" => "text-x-vcard",
        "ics" | "ifb" => "text-x-vcalendar",

        // ── System ────────────────────────────────────────────────────
        "so" | "dll" | "dylib" | "a" | "o" | "ko" => "application-x-executable",
        "efi" => "application-x-executable",
        "bin" => "application-x-executable",
        "run" | "sh1" => "application-x-executable",
        "torrent" => "application-x-bittorrent",
        "ipynb" => "text-x-python",

        _ => return None,
    })
}

/// Icon name for a file with no extension, or one this table does not name.
///
/// The executable bit is the reliable signal here rather than a list of
/// extensions: `AppImage`, `run`, `bin` and a bare `foo` are all the same kind
/// of thing to a file manager, and adbshare already has the mode in the
/// directory listing. Also the reason a desktop-installable binary with no
/// extension reads as a program rather than as a mystery blob.
pub fn name_for_mode(ext: &str, mode: u32) -> Option<&'static str> {
    if let Some(name) = name_for(ext) {
        return Some(name);
    }
    if mode & 0o111 != 0 {
        return Some("application-x-executable");
    }
    None
}

/// The theme icon for a file, given its full path.
///
/// The path is only consulted for a file with no extension: that is the one case
/// where neither the name nor the mode can identify it, and the case where a
/// file manager falls back to reading the content.
pub fn name_for_path(ext: &str, mode: u32, path: &std::path::Path) -> Option<&'static str> {
    name_for_mode(ext, mode).or_else(|| if ext.is_empty() { sniff(path) } else { None })
}

// ── Content sniffing ─────────────────────────────────────────────────────────

/// Icon name for a file whose name says nothing.
///
/// Nautilus answers this from the file's *content*, not its name or its mode:
/// `circle-to-search` in a Downloads folder is an ELF binary with no extension
/// and mode 644, and the file manager beside it draws a program badge for it
/// while the permission bit says otherwise. GIO does the same, calling it
/// unguessable content.
///
/// Eight bytes covers every magic worth matching, and a shorter file is compared
/// against whatever it does have rather than rejected — an empty or two-byte
/// file is not a reason to skip the check. The per-path cache means this reads
/// once per file per session, and it only runs for a file with no extension,
/// which is rare enough that the cache is belt and braces rather than the thing
/// keeping it off the render path.
fn sniff(path: &std::path::Path) -> Option<&'static str> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<&'static str>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cache.get(path) {
        return *hit;
    }

    // Read into an owned buffer so the slice handed to the matcher does not
    // borrow from a temporary that the closure would outlive.
    let head: Option<Vec<u8>> = std::fs::File::open(path).ok().and_then(|mut f| {
        use std::io::Read;
        let mut buf = [0u8; 8];
        f.read(&mut buf).ok().map(|read| buf[..read].to_vec())
    });

    const EXEC: &str = "application-x-executable";
    let verdict = head.as_deref().and_then(|h| {
        Some(match h {
            h if h.starts_with(b"\x7fELF") => EXEC,
            h if h.starts_with(b"#!") => "text-x-script",
            h if h.starts_with(b"PK\x03\x04") => "application-zip",
            h if h.starts_with(b"PK\x05\x06") => "application-x-archive",
            h if h.starts_with(b"\x1f\x8b") => "application-gzip",
            h if h.starts_with(b"BZh") => "application-x-archive",
            h if h.starts_with(b"\xfd7zXZ\x00") => "application-x-archive",
            h if h.starts_with(b"\x28\xb5\x2f\xfd") => "application-zstd",
            h if h.starts_with(b"\x89PNG") => "image-x-generic",
            h if h.starts_with(b"\xff\xd8\xff") => "image-x-generic",
            h if h.starts_with(b"GIF8") => "image-x-generic",
            h if h.starts_with(b"RIFF") => "image-x-generic",
            h if h.starts_with(b"fLaC") => "audio-x-generic",
            h if h.starts_with(b"ID3") => "audio-x-generic",
            h if h.starts_with(b"\x1a\x45\xdf\xa3") => "video-x-generic",
            _ => return None,
        })
    });

    cache.insert(path.to_path_buf(), verdict);
    verdict
}

// ── Theme lookup ─────────────────────────────────────────────────────────────

/// Where to look for themes, in order.
///
/// The icon theme spec puts `hicolor` last as the guaranteed fallback, and
/// every other installed theme ahead of it. All the themes that ship these
/// names agree on them, so the order between them barely matters — what matters
/// is that `hicolor` is a fallback rather than the only place looked.
fn theme_dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Some(home) = dirs::data_dir() {
            dirs.push(home.join("icons"));
        }
        match std::env::var("XDG_DATA_DIRS") {
            Ok(list) => dirs.extend(std::env::split_paths(&list).map(|d| d.join("icons"))),
            Err(_) => {
                dirs.push(PathBuf::from("/usr/local/share/icons"));
                dirs.push(PathBuf::from("/usr/share/icons"));
            }
        }
        dirs
    })
}

/// The icon theme the desktop is configured to use.
///
/// Read rather than guessed, because getting it wrong does not fail — it
/// silently produces a *different* theme's icon. This machine runs `Yaru-purple`,
/// which ships no icons of its own and inherits `Yaru`, so a lookup that skipped
/// the theme name drew an executable badge out of whichever unrelated theme
/// happened to be enumerated first.
fn configured_theme() -> Option<String> {
    // GTK's own settings files first: a plain read, and the same value GTK uses.
    for version in ["4.0", "3.0"] {
        let path = dirs::config_dir()?.join(format!("gtk-{version}/settings.ini"));
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if let Some(value) = line
                .strip_prefix("gtk-icon-theme-name")
                .and_then(|rest| rest.trim_start().strip_prefix('='))
            {
                let name = value.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    // Otherwise ask the settings daemon, which is where GNOME keeps it when no
    // GTK settings file was ever written. Cheap, once, and cached.
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        .ok()?;
    let value = String::from_utf8(out.stdout).ok()?;
    let name = value.trim().trim_matches('\'').trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Theme directories to search, in order, for the configured theme and
/// everything it inherits.
///
/// `Inherits=Yaru,hicolor` is what makes this necessary: a theme may be a pure
/// recolour that owns no artwork, so its parents hold the files. Breadth-first
/// over the chain, so the configured theme always wins.
fn theme_chain() -> &'static [PathBuf] {
    static CHAIN: OnceLock<Vec<PathBuf>> = OnceLock::new();
    CHAIN.get_or_init(|| {
        let mut ordered = Vec::new();
        let mut queue: Vec<String> = configured_theme().into_iter().collect();
        let mut seen: Vec<String> = Vec::new();
        while let Some(theme) = queue.first().cloned() {
            queue.remove(0);
            if seen.contains(&theme) {
                continue;
            }
            seen.push(theme.clone());
            for base in theme_dirs() {
                let dir = base.join(&theme);
                let index = dir.join("index.theme");
                let text = std::fs::read_to_string(&index).unwrap_or_default();
                // `Inherits` goes after the theme itself, per the spec.
                if let Some(parents) = text
                    .lines()
                    .find_map(|l| l.strip_prefix("Inherits=").map(|v| v.trim().to_string()))
                {
                    queue.extend(parents.split(',').map(|p| p.trim().to_string()));
                }
                ordered.push(dir);
            }
        }
        ordered
    })
}

/// Raster sizes to try, nearest first.
///
/// Themes do not carry every size for every icon, and a theme that only ships
/// 48x48 is still a better answer than the generic page. So this walks down to
/// 16 and then back up, rather than demanding an exact match — and it stops at
/// 16 because below that every `text-x-*` glyph is an unreadable smudge.
fn size_ladder(want: u32) -> Vec<u32> {
    let want = want.clamp(16, 256);
    let down = (1..=want / 16).rev().map(|n| n * 16);
    let up = (want / 16 + 1..=256 / 16).map(|n| n * 16);
    down.chain(up).collect()
}

/// The first raster of `name` any installed theme provides.
///
/// The layout is `$icons/<theme>/<size>/<subdir>/<name>.png`, and `subdir` is
/// whatever the theme felt like: `mimetypes/` for most types, but `devices/`
/// for the disc and flash-drive art, `places/` for folders. So every
/// subdirectory at the requested size is searched rather than a hardcoded list,
/// which is also what an icon lookup proper does.
fn find_icon_file(name: &str, want: u32) -> Option<PathBuf> {
    find_in(theme_chain(), name, want).or_else(|| find_in(all_theme_dirs(), name, want))
}

/// Every theme directory installed, for when the configured chain has nothing.
fn all_theme_dirs() -> &'static [PathBuf] {
    static ALL: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut dirs = Vec::new();
        for base in theme_dirs() {
            let Ok(themes) = std::fs::read_dir(base) else {
                continue;
            };
            dirs.extend(themes.flatten().map(|t| t.path()).filter(|p| p.is_dir()));
        }
        dirs
    })
}

fn find_in(theme_dirs: &[PathBuf], name: &str, want: u32) -> Option<PathBuf> {
    let file = format!("{name}.png");
    {
        for theme_dir in theme_dirs {
            if !theme_dir.is_dir() {
                continue;
            }
            for size in size_ladder(want) {
                let size_dir = theme_dir.join(format!("{size}x{size}"));
                let Ok(entries) = std::fs::read_dir(&size_dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    // The size directory itself may hold the file outright.
                    if entry.file_name() == file.as_str() {
                        return Some(entry.path());
                    }
                    let sub = entry.path();
                    if sub.is_dir() && sub.join(&file).is_file() {
                        return Some(sub.join(&file));
                    }
                }
            }
        }
    }
    None
}

/// Decoded theme PNG, shared so a repeated request does not re-read the file.
type IconBytes = Arc<Vec<u8>>;

/// The icon cache: icon name and requested size to the bytes, if any theme has
/// them.
type IconCache = HashMap<(&'static str, u32), Option<IconBytes>>;

/// PNG bytes for `name`, or `None` when no installed theme has it.
///
/// Cached, because the grid asks for the same handful of icons on every frame
/// of every folder. A miss is cached too: a name no theme has will not appear
/// between two frames, and re-walking nine theme directories per tile per frame
/// to learn that is the kind of cost that shows up as the window stuttering
/// while scrolling.
fn icon_bytes(name: &'static str, want: u32) -> Option<Arc<Vec<u8>>> {
    static CACHE: OnceLock<Mutex<IconCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cache.get(&(name, want)) {
        return hit.clone();
    }
    let loaded = find_icon_file(name, want)
        .and_then(|p| std::fs::read(p).ok())
        .map(Arc::new);
    cache.insert((name, want), loaded.clone());
    loaded
}

/// The themed icon for a file, if the desktop has one.
///
/// `size` is the pixel size to draw at; the theme's own raster is picked to
/// match so the GPU downsamples instead of the CPU upsampling.
pub fn element(ext: &str, mode: u32, size: f32, path: &std::path::Path) -> Option<AnyElement> {
    let name = name_for_path(ext, mode, path)?;
    let want = size.round().max(16.0) as u32;
    let bytes = icon_bytes(name, want)?;
    // The bytes came from a `.png`, so the format is not a guess.
    let image = Arc::new(Image::from_bytes(ImageFormat::Png, (*bytes).clone()));
    Some(
        gpui::img(gpui::ImageSource::Image(image))
            .size(gpui::px(size))
            .object_fit(gpui::ObjectFit::Contain)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extension to icon name for every family the table covers.
    ///
    /// One list, checked in both directions: each pair must be what the table
    /// actually returns *and* the name must exist in an installed theme. Two
    /// separate lists would drift, and the second list is the one that would
    /// quietly stop testing anything.
    const TABLE: &[(&str, &str)] = &[
        // Documents
        ("pdf", "application-pdf"),
        ("ps", "application-postscript"),
        ("djvu", "x-office-document"),
        ("epub", "application-epub+zip"),
        ("rtf", "application-rtf"),
        ("tex", "text-x-tex"),
        // Office
        ("odt", "application-vnd.oasis.opendocument.text"),
        ("ods", "application-vnd.oasis.opendocument.spreadsheet"),
        ("csv", "application-vnd.oasis.opendocument.spreadsheet"),
        ("odp", "application-vnd.oasis.opendocument.presentation"),
        ("odg", "application-vnd.oasis.opendocument.graphics"),
        ("doc", "application-msword"),
        (
            "docx",
            "application-vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        ("xls", "application-vnd.ms-excel"),
        (
            "xlsx",
            "application-vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        ("ppt", "application-vnd.ms-powerpoint"),
        (
            "pptx",
            "application-vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        // Text and markup
        ("txt", "text-x-generic"),
        ("md", "text-markdown"),
        ("log", "text-x-log"),
        ("html", "text-html"),
        ("css", "text-css"),
        ("xml", "text-xml"),
        ("json", "application-json"),
        ("toml", "application-toml"),
        ("yaml", "application-yaml"),
        ("sql", "application-sql"),
        ("patch", "text-x-patch"),
        // Code
        ("py", "text-x-python"),
        ("rs", "text-rust"),
        ("js", "text-x-javascript"),
        ("ts", "text-x-typescript"),
        ("c", "text-x-csrc"),
        ("cpp", "text-x-c++src"),
        ("cs", "text-x-csharp"),
        ("java", "text-x-java"),
        ("go", "text-x-go"),
        ("rb", "text-x-ruby"),
        ("php", "text-x-php"),
        ("sh", "text-x-script"),
        ("lua", "text-x-lua"),
        ("hs", "text-x-haskell"),
        ("scala", "text-x-scala"),
        ("dart", "text-x-script"),
        ("ml", "text-x-script"),
        ("f90", "text-x-fortran"),
        ("asm", "text-x-script"),
        ("cmake", "text-x-makefile"),
        ("service", "text-x-systemd-unit"),
        // Archives and packages
        ("zip", "application-zip"),
        ("tar", "application-x-archive"),
        ("gz", "application-gzip"),
        ("bz2", "application-x-archive"),
        ("xz", "application-x-archive"),
        ("zst", "application-x-archive"),
        ("7z", "application-x-7z-compressed"),
        ("rar", "application-rar"),
        ("iso", "media-optical"),
        ("deb", "application-vnd.debian.binary-package"),
        ("apk", "application-apk"),
        ("snap", "application-vnd.snap"),
        ("appimage", "application-x-executable"),
        ("exe", "application-vnd.microsoft.portable-executable"),
        ("jar", "application-x-java-archive"),
        // Media
        ("png", "image-x-generic"),
        ("svg", "image-x-generic"),
        ("mp3", "audio-x-generic"),
        ("flac", "audio-x-generic"),
        ("mkv", "video-x-generic"),
        ("mp4", "video-x-generic"),
        // Fonts, keys, system
        ("ttf", "font-x-generic"),
        ("gpg", "application-pgp-keys"),
        ("so", "application-x-executable"),
        ("torrent", "application-x-bittorrent"),
    ];

    /// A misspelled icon name does not fail — it silently falls through to the
    /// generic page, which is exactly the bug this feature exists to remove. So
    /// every name the table can produce is resolved against the themes actually
    /// installed on the machine running the test.
    #[test]
    fn every_name_resolves_on_this_machine() {
        for (ext, name) in TABLE {
            assert_eq!(name_for(ext), Some(*name), "{ext} maps elsewhere");
            assert!(
                find_icon_file(name, 128).is_some(),
                "no installed theme provides {name}.png (for .{ext})"
            );
        }
    }

    /// The executable bit is the one thing a table of extensions cannot answer,
    /// and it is how Nautilus labels an extensionless AppImage or `run` script.
    #[test]
    fn the_executable_bit_catches_what_no_extension_would() {
        assert_eq!(name_for_mode("", 0o755), Some("application-x-executable"));
        assert_eq!(name_for_mode("", 0o644), None);
        assert_eq!(name_for_mode("xyz", 0o644), None);
        // A known extension still wins over the bit.
        assert_eq!(name_for_mode("pdf", 0o755), Some("application-pdf"));
    }

    #[test]
    fn the_size_ladder_prefers_the_nearest_raster() {
        let ladder = size_ladder(128);
        assert_eq!(ladder[0], 128);
        // Descends to 16, then climbs: the near rasters first, then whatever is
        // larger, in that order.
        let floor = ladder.iter().position(|s| *s == 16).expect("reaches 16");
        assert!(
            ladder[..=floor].windows(2).all(|w| w[0] > w[1]),
            "must descend to 16"
        );
        assert!(
            ladder[floor + 1..].windows(2).all(|w| w[0] < w[1]),
            "must climb after 16"
        );
        // 128 is missing from some themes and 64 is where it lands.
        assert!(ladder.contains(&64));
        // Never below 16: every text-x-* glyph turns to mush.
        assert!(ladder.iter().all(|s| *s >= 16));
        // And it climbs when nothing smaller exists.
        assert!(size_ladder(16).iter().any(|s| *s > 16));
    }
}

#[cfg(test)]
mod sniff_tests {
    use super::*;

    /// The point of sniffing: a binary with no extension and no executable bit
    /// still has to be recognisable, which is the case a name and a mode both
    /// fail.
    #[test]
    fn an_extensionless_binary_is_still_a_program() {
        let dir = std::env::temp_dir().join("adbshare-sniff-test");
        std::fs::create_dir_all(&dir).unwrap();

        let elf = dir.join("no-name-program");
        std::fs::write(&elf, b"\x7fELF\x02\x01\x01").unwrap();
        assert_eq!(
            name_for_path("", 0o644, &elf),
            Some("application-x-executable")
        );

        let script = dir.join("no-name-script");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(name_for_path("", 0o644, &script), Some("text-x-script"));

        let zip = dir.join("no-name-archive");
        std::fs::write(&zip, b"PK\x03\x04rest").unwrap();
        assert_eq!(name_for_path("", 0o644, &zip), Some("application-zip"));

        // A name that says nothing and content that says nothing stays unknown.
        let plain = dir.join("no-name-text");
        std::fs::write(&plain, b"just some words").unwrap();
        assert_eq!(name_for_path("", 0o644, &plain), None);

        // An extension short-circuits sniffing entirely: a .txt full of ELF
        // bytes is still a text file by name, and reading it would be a syscall
        // per frame for nothing.
        assert_eq!(name_for_path("txt", 0o644, &elf), Some("text-x-generic"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
