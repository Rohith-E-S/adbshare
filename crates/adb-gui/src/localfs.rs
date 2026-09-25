//! Local filesystem operations for the "This computer" browsing mode.
//!
//! The GTK build leaned on GLib for two of these: `gio::File::trash_future`
//! for "Move to Trash" and `gio::Notification` for device connect/disconnect
//! toasts. Pulling GLib in behind a GPUI window would drag a second main loop
//! and a second set of platform libraries into the process, so both are done
//! directly here: the XDG Trash spec is short, and toasts live in
//! [`crate::toast`].

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::protocol::DirEntry;

/// Guard against pathological trees during a recursive local copy, matching the
/// daemon's own limits.
const MAX_COPY_FILES: usize = 10_000;
const MAX_COPY_DEPTH: u32 = 32;

/// List a local directory, folders first then case-insensitive by name.
///
/// Every entry is returned including dotfiles: the browser's "Show hidden
/// files" toggle filters them, so filtering here would make the toggle a no-op
/// in local mode.
pub fn list_dir(path: &Path) -> Result<Vec<DirEntry>, String> {
    let read_dir = fs::read_dir(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut out = Vec::new();

    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // `DirEntry::metadata` does not follow symlinks, so stat the target
        // first: otherwise a symlinked directory such as /bin -> usr/bin
        // would draw as a file.
        let meta = fs::metadata(entry.path()).or_else(|_| entry.metadata());
        let Ok(meta) = meta else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        out.push(DirEntry {
            name,
            is_dir: meta.is_dir(),
            is_symlink: entry.file_type().is_ok_and(|t| t.is_symlink()),
            size: meta.len(),
            mode: meta.permissions().mode() & 0o7777,
            mtime,
        });
    }

    sort_entries(&mut out);
    Ok(out)
}

/// Folders first, then case-insensitive by name.
///
/// The daemon already returns its listings in that order; local ones are sorted
/// here so both views order identically.
pub fn sort_entries(entries: &mut Vec<DirEntry>) {
    crate::protocol::sort_by_folder_then_name(entries);
}

/// Create one directory, including parents.
pub fn mkdir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// Rename or move a path.
pub fn rename(src: &Path, dst: &Path) -> io::Result<()> {
    fs::rename(src, dst)
}

/// Delete a path permanently. Used for Shift+Delete; the plain Delete key goes
/// through [`trash`] so it can be undone from the desktop's trash.
pub fn delete(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Copy one file, creating the destination's parent directory.
pub fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst)?;
    // Preserve the executable bit, which `fs::copy` does not carry over.
    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(dst, meta.permissions());
    }
    Ok(())
}

/// Recursively copy a directory tree, returning the number of files copied.
///
/// Iterative rather than recursive so a deep tree cannot blow the stack.
/// Symlinks are skipped: following them risks loops and duplicating data
/// outside the source tree.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<usize> {
    let mut copied = 0;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf(), 0u32)];

    while let Some((from, to, depth)) = stack.pop() {
        if depth > MAX_COPY_DEPTH {
            return Err(io::Error::other(format!(
                "{}: nesting too deep",
                from.display()
            )));
        }
        fs::create_dir_all(&to)?;
        for entry in fs::read_dir(&from)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            let target = to.join(entry.file_name());
            if file_type.is_dir() {
                stack.push((entry.path(), target, depth + 1));
            } else if file_type.is_file() {
                if copied >= MAX_COPY_FILES {
                    return Err(io::Error::other(format!(
                        "too many files (limit {MAX_COPY_FILES})"
                    )));
                }
                copy_file(&entry.path(), &target)?;
                copied += 1;
            }
        }
    }
    Ok(copied)
}

// ── Trash ────────────────────────────────────────────────────────────────────

/// Move a path to the XDG trash directory, returning where it landed.
///
/// Implements the freedesktop.org Trash specification directly: the file moves
/// to `~/.local/share/Trash/files` and a matching `.trashinfo` file records the
/// original location and deletion time so a desktop file manager can offer
/// "Restore".
pub fn trash(path: &Path) -> Result<PathBuf, String> {
    let data_home = dirs::data_dir().ok_or_else(|| "no XDG data directory".to_string())?;
    let trash_root = data_home.join("Trash");
    let files_dir = trash_root.join("files");
    let info_dir = trash_root.join("info");
    fs::create_dir_all(&files_dir).map_err(|e| format!("{}: {e}", files_dir.display()))?;
    fs::create_dir_all(&info_dir).map_err(|e| format!("{}: {e}", info_dir.display()))?;

    // Disambiguate collisions by appending a counter, matching the spec.
    let stem = path
        .file_name()
        .ok_or_else(|| format!("{}: no file name", path.display()))?
        .to_string_lossy()
        .to_string();
    let (target, trash_name) = unique_name(&files_dir, &stem);
    let info_target = info_dir.join(format!("{trash_name}.trashinfo"));

    let absolute = fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let now = chrono::Local::now();
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        escape_percent(&absolute.to_string_lossy()),
        now.format("%Y-%m-%dT%H:%M:%S")
    );

    // Write the record first: an orphaned .trashinfo is recoverable, a moved
    // file with no record looks like data loss to the trash UI.
    fs::write(&info_target, info).map_err(|e| format!("{}: {e}", info_target.display()))?;
    if let Err(err) = move_path(path, &target) {
        // Roll the record back so a failed move leaves nothing behind.
        let _ = fs::remove_file(&info_target);
        return Err(format!("{}: {err}", path.display()));
    }
    Ok(target)
}

/// Append a counter to `stem` until the name is free in `dir`.
fn unique_name(dir: &Path, stem: &str) -> (PathBuf, String) {
    let mut candidate = stem.to_string();
    for n in 1..10_000u32 {
        let target = dir.join(&candidate);
        if !target.exists() {
            return (target, candidate);
        }
        candidate = format!("{stem}.{n}");
    }
    // Pathological directory; fall back to something guaranteed unique.
    let candidate = format!("{stem}.{}", std::process::id());
    (dir.join(&candidate), candidate)
}

/// `move_path` on the same filesystem is a rename; across filesystems it has to
/// copy then delete, so try the cheap case first.
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        // EXDEV, the errno for a cross-device rename.
        Err(err) if err.raw_os_error() == Some(18) => {
            let meta = fs::symlink_metadata(src)?;
            if meta.is_dir() {
                copy_tree(src, dst)?;
                fs::remove_dir_all(src)
            } else {
                copy_file(src, dst)?;
                fs::remove_file(src)
            }
        }
        Err(err) => Err(err),
    }
}

/// The Trash Info format requires `%`, `\n` and `\` in the path to be escaped.
fn escape_percent(s: &str) -> String {
    s.replace('%', "%25")
        .replace('\n', "%0A")
        .replace('\\', "%5C")
}

// ── Platform helpers ─────────────────────────────────────────────────────────

/// Open a path with the desktop's default handler.
pub fn open_external(path: &Path) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("xdg-open: {e}"))
}

/// Open a terminal in a directory, honouring the desktop's preference and
/// falling back to the common emulators.
pub fn open_terminal(dir: &Path) -> Result<(), String> {
    let candidates: &[(&str, &[&str])] = &[
        ("gnome-terminal", &["--working-directory"]),
        ("kgx", &["--working-directory"]),
        ("xfce4-terminal", &["--working-directory"]),
        ("x-terminal-emulator", &["--working-directory"]),
        ("konsole", &["--workdir"]),
        ("alacritty", &["--working-directory"]),
        ("kitty", &["--directory"]),
    ];

    let path = dir.display().to_string();
    let mut tried = Vec::new();
    for (program, flag) in candidates {
        match std::process::Command::new(*program)
            .args(flag.iter().copied())
            .arg(&path)
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(err) => tried.push(format!("{program}: {err}")),
        }
    }
    Err(format!("no terminal emulator found ({})", tried.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "adbshare-localfs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).expect("temp dir");
        base
    }

    fn seed_tree(base: &Path) -> (PathBuf, PathBuf) {
        let src = base.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"a").unwrap();
        fs::write(src.join("sub").join("b.txt"), b"b").unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
        (src, base.join("dst"))
    }

    #[test]
    fn copy_tree_copies_hierarchy_and_skips_symlinks() {
        let base = temp_dir("copy");
        let (src, dst) = seed_tree(&base);

        let copied = copy_tree(&src, &dst).expect("copy succeeds");

        assert_eq!(copied, 2, "the symlink must not be followed");
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(dst.join("sub").join("b.txt")).unwrap(), b"b");
        assert!(
            !dst.join("link").exists(),
            "symlinks are skipped, not copied"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn copy_tree_preserves_the_executable_bit() {
        let base = temp_dir("exec");
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        let script = src.join("run.sh");
        fs::write(&script, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let dst = base.join("dst");
        copy_tree(&src, &dst).expect("copy succeeds");

        let mode = fs::metadata(dst.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "got {:o}", mode & 0o777);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn copy_tree_refuses_runaway_depth() {
        let base = temp_dir("deep");
        let src = base.join("src");
        // Build a chain deeper than the limit.
        let mut here = src.clone();
        fs::create_dir_all(&here).unwrap();
        for _ in 0..(MAX_COPY_DEPTH + 2) {
            here = here.join("d");
            fs::create_dir_all(&here).unwrap();
        }
        fs::write(here.join("leaf"), b"x").unwrap();

        let err = copy_tree(&src, &base.join("out")).expect_err("depth is capped");
        assert!(err.to_string().contains("nesting too deep"), "{err}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn list_dir_returns_dotfiles_and_orders_folders_first() {
        let base = temp_dir("list");
        fs::create_dir_all(base.join("zdir")).unwrap();
        fs::create_dir_all(base.join("Adir")).unwrap();
        fs::write(base.join("b.txt"), b"b").unwrap();
        fs::write(base.join(".hidden"), b"h").unwrap();

        let entries = list_dir(&base).expect("list succeeds");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&".hidden"), "{names:?}");
        assert_eq!(
            &names[..2],
            &["Adir", "zdir"],
            "folders first, case-insensitive"
        );
        // Among files, '.' sorts before letters, so dotfiles lead — the same
        // order Nautilus shows them in.
        assert_eq!(&names[2..], &[".hidden", "b.txt"], "{names:?}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn list_dir_reports_a_missing_directory() {
        let err = list_dir(Path::new("/definitely/not/here")).expect_err("must fail");
        assert!(err.contains("/definitely/not/here"), "{err}");
    }

    #[test]
    fn trash_info_escapes_special_characters() {
        assert_eq!(escape_percent("/a%b/c"), "/a%25b/c");
        assert_eq!(escape_percent("/a\\b"), "/a%5Cb");
        assert_eq!(escape_percent("line\nbreak"), "line%0Abreak");
    }

    #[test]
    fn delete_removes_files_and_directories() {
        let base = temp_dir("delete");
        let file = base.join("f.txt");
        fs::write(&file, b"x").unwrap();
        delete(&file).expect("file deleted");
        assert!(!file.exists());

        let dir = base.join("d");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested").join("f"), b"x").unwrap();
        delete(&dir).expect("dir deleted");
        assert!(!dir.exists());
        fs::remove_dir_all(&base).ok();
    }
}
