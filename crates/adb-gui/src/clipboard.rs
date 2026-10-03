//! Copy and paste of files, across the local disk and between phones.
//!
//! GPUI's clipboard only speaks strings and images, so file selections are
//! mirrored to the system clipboard as newline-separated absolute paths. That
//! is the same convention GTK used for `text/uri-list`, which means a path
//! copied here can still be pasted into a terminal or another file manager.
//!
//! The authoritative snapshot is [`ClipboardFiles`], held by the app: the system
//! clipboard can be overwritten by anything else, but a Ctrl+C inside the app
//! should never be.

use std::path::{Path, PathBuf};

use gpui::{App, ClipboardItem};

use crate::protocol::{ClipboardFiles, DirEntry};

/// The MIME type recorded alongside the mirrored path list, so a reader can tell
/// a file selection from loose text.
pub const PATH_MIME: &str = "adbshare/x-file-paths";

/// Write the copied selection to the system clipboard as absolute paths.
pub fn mirror_to_system(cx: &App, entries: &[DirEntry], from_dir: &Path) {
    let paths = absolute_paths(entries, from_dir);
    if paths.is_empty() {
        return;
    }
    let text = paths
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
        text,
        PATH_MIME.to_string(),
    ));
}

/// Read a path list back off the system clipboard.
///
/// Only returns paths that look absolute: pasting loose text into a file
/// manager should do nothing rather than guess.
pub fn paths_from_system(cx: &App) -> Vec<PathBuf> {
    let Some(item) = cx.read_from_clipboard() else {
        return Vec::new();
    };
    let Some(text) = item.text() else {
        return Vec::new();
    };
    decode(&text)
}

/// Join the entries onto the directory they were copied from.
///
/// Entries from a device listing are already absolute (`/sdcard/...`); local
/// entries are bare names and need the browsed directory.
pub fn absolute_paths(entries: &[DirEntry], from_dir: &Path) -> Vec<PathBuf> {
    entries
        .iter()
        .map(|entry| {
            let name = Path::new(&entry.name);
            if name.is_absolute() {
                name.to_path_buf()
            } else {
                from_dir.join(name)
            }
        })
        .collect()
}

/// Split a mirrored clipboard payload into absolute paths, ignoring blanks and
/// anything that is not absolute.
///
/// Accepts both conventions a file manager might leave on the clipboard: bare
/// absolute paths, and `file://` URIs. The URI form is what Nautilus, Dolphin
/// and `gio` actually copy, and dropping it made pasting between file managers
/// silently do nothing.
pub fn decode(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let path = from_uri(line).unwrap_or_else(|| PathBuf::from(line));
            path.is_absolute().then_some(path)
        })
        .collect()
}

/// Resolve one clipboard line to a path, decoding a `file://` URI if it is one.
///
/// A URI is only accepted with an empty or `localhost` authority, because
/// `file://somehost/share/x` names a path on another machine and this process
/// cannot read it.
fn from_uri(line: &str) -> Option<PathBuf> {
    let rest = line.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        // `file://localhost/x` and `file:///x` are the same path.
        Some(_) if rest[..rest.find('/').unwrap()].eq_ignore_ascii_case("localhost") => {
            &rest[rest.find('/').unwrap()..]
        }
        Some(_) => return None,
        None => return None,
    };
    Some(PathBuf::from(percent_decode(path)))
}

/// Decode `%XX` escapes in a URI path.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a clipboard snapshot against the directory being browsed now,
/// returning the destination paths the transfer would write to.
///
/// Mirrors the GTK build's `paste_clipboard` argument validation: a snapshot
/// taken on a different device than the one being browsed is refused outright,
/// because pushing one phone's `/sdcard` onto another would silently clobber.
pub fn resolve_destination(
    snapshot: &ClipboardFiles,
    target_dir: &Path,
    target_device: Option<&str>,
) -> Result<Vec<PathBuf>, String> {
    if let (Some(origin), Some(target)) = (snapshot.device.as_deref(), target_device)
        && origin != target
    {
        return Err(format!(
            "These files were copied from {origin}. Paste into that device, or copy them again."
        ));
    }
    let sources = absolute_paths(&snapshot.entries, &snapshot.from_dir);
    let destinations = sources
        .into_iter()
        .map(|src| {
            let name = src
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("untitled"));
            target_dir.join(name)
        })
        .collect();
    Ok(destinations)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> DirEntry {
        DirEntry {
            name: name.into(),
            is_dir,
            is_symlink: false,
            size: 0,
            mode: 0o644,
            mtime: 0,
        }
    }

    fn snapshot(from_local: bool, device: Option<&str>, from_dir: &str) -> ClipboardFiles {
        ClipboardFiles {
            from_local,
            device: device.map(str::to_string),
            from_dir: PathBuf::from(from_dir),
            entries: vec![entry("a.txt", false), entry("sub", true)],
        }
    }

    #[test]
    fn absolute_paths_leaves_device_paths_alone() {
        let entries = vec![entry("/sdcard/Download/a.txt", false)];
        let out = absolute_paths(&entries, Path::new("/home/me"));
        assert_eq!(out, vec![PathBuf::from("/sdcard/Download/a.txt")]);
    }

    #[test]
    fn absolute_paths_joins_local_names_onto_the_browsed_directory() {
        let entries = vec![entry("a.txt", false), entry("sub", true)];
        let out = absolute_paths(&entries, Path::new("/home/me/Downloads"));
        assert_eq!(
            out,
            vec![
                PathBuf::from("/home/me/Downloads/a.txt"),
                PathBuf::from("/home/me/Downloads/sub"),
            ]
        );
    }

    #[test]
    fn decode_keeps_only_absolute_paths() {
        let text = "/home/me/a.txt\n\n  /home/me/b.txt  \nrelative.txt\n/tmp/";
        assert_eq!(
            decode(text),
            vec![
                PathBuf::from("/home/me/a.txt"),
                PathBuf::from("/home/me/b.txt"),
                PathBuf::from("/tmp/"),
            ]
        );
        assert!(decode("").is_empty());
    }

    #[test]
    fn decode_reads_the_file_uris_a_file_manager_leaves() {
        let text = concat!(
            "# This is a URI list as Nautilus writes it.\n",
            "file:///home/me/My%20Documents/report.pdf\n",
            "file://localhost/home/me/plain.txt\n",
        );
        assert_eq!(
            decode(text),
            vec![
                PathBuf::from("/home/me/My Documents/report.pdf"),
                PathBuf::from("/home/me/plain.txt"),
            ]
        );
    }

    #[test]
    fn decode_refuses_a_uri_that_names_another_host() {
        // `file://nas/share/x` is a path on another machine; pretending it is
        // local would hand the transfer engine a path that cannot be read.
        assert!(decode("file://nas/share/x").is_empty());
        assert!(decode("file://").is_empty());
    }

    #[test]
    fn decode_mixes_bare_paths_and_uris() {
        let text = "/tmp/a.txt\nfile:///tmp/b%20c.txt\nhttp://example.com/x\nnot absolute";
        assert_eq!(
            decode(text),
            vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt"),]
        );
    }

    #[test]
    fn percent_decoding_survives_a_malformed_escape() {
        assert_eq!(percent_decode("/a%2"), "/a%2");
        assert_eq!(percent_decode("/a%zz"), "/a%zz");
        assert_eq!(percent_decode("/a%20b"), "/a b");
    }

    #[test]
    fn destination_resolution_appends_the_source_file_name() {
        let snap = snapshot(false, Some("pixel"), "/sdcard/Download");
        let out = resolve_destination(&snap, Path::new("/sdcard/DCIM"), Some("pixel"))
            .expect("same device is allowed");
        assert_eq!(
            out,
            vec![
                PathBuf::from("/sdcard/DCIM/a.txt"),
                PathBuf::from("/sdcard/DCIM/sub"),
            ]
        );
    }

    #[test]
    fn pasting_across_devices_is_refused() {
        let snap = snapshot(false, Some("pixel"), "/sdcard/Download");
        let err = resolve_destination(&snap, Path::new("/sdcard/DCIM"), Some("tablet"))
            .expect_err("a different device must be refused");
        assert!(err.contains("pixel"), "{err}");
    }

    #[test]
    fn pasting_a_device_snapshot_while_browsing_locally_is_allowed() {
        // Local mode passes `None` for the target device, so there is nothing to
        // conflict with; the caller turns these into a download.
        let snap = snapshot(false, Some("pixel"), "/sdcard/Download");
        let out = resolve_destination(&snap, Path::new("/home/me/Downloads"), None)
            .expect("no device conflict");
        assert_eq!(
            out,
            vec![
                PathBuf::from("/home/me/Downloads/a.txt"),
                PathBuf::from("/home/me/Downloads/sub"),
            ]
        );
    }
}
