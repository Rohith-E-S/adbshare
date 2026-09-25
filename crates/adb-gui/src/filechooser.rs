//! Native file pickers, via the XDG desktop portal.
//!
//! The GTK build used `gtk4::FileChooserNative`, which under Wayland is itself
//! a portal client, so going straight to `ashpd` preserves both the look of the
//! system dialog and its sandbox behaviour (including Flatpak's document portal)
//! without pulling GLib in behind a GPUI window.
//!
//! The portal calls are async, so like the D-Bus client they are dispatched
//! onto the pinned tokio runtime and awaited from a GPUI task.

use std::path::PathBuf;

use ashpd::desktop::file_chooser::{
    FileChooserProxy, FileFilter, OpenFileOptions, SaveFileOptions,
};

/// What to ask the user for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// One or more existing files.
    Files,
    /// One existing directory.
    Folder,
}

/// A completed pick; `None` means the user cancelled.
pub type Picked = Option<Vec<PathBuf>>;

/// Ask for existing files or a directory.
pub async fn pick(kind: Pick, title: &str) -> Result<Picked, String> {
    let title = title.to_string();
    crate::daemon::on_tokio(move || async move {
        let proxy = FileChooserProxy::new().await.map_err(|e| e.to_string())?;
        let options = OpenFileOptions::default()
            .set_multiple(true)
            .set_directory(matches!(kind, Pick::Folder))
            .set_modal(true)
            .set_filters([FileFilter::new("All files").glob("*")]);

        let request = proxy
            .open_file(None, &title, options)
            .await
            .map_err(|e| e.to_string())?;
        let response = request.response().map_err(|e| e.to_string())?;
        Ok(Some(uris_to_paths(response.uris())))
    })
    .await
}

/// Ask where to save a file, with `suggested` pre-filling the name field.
pub async fn save(title: &str, suggested: &str) -> Result<Picked, String> {
    let (title, suggested) = (title.to_string(), suggested.to_string());
    crate::daemon::on_tokio(move || async move {
        let proxy = FileChooserProxy::new().await.map_err(|e| e.to_string())?;
        let options = SaveFileOptions::default()
            .set_modal(true)
            .set_current_name(Some(suggested.as_str()))
            .set_filters([FileFilter::new("All files").glob("*")]);

        let request = proxy
            .save_file(None, &title, options)
            .await
            .map_err(|e| e.to_string())?;
        let response = request.response().map_err(|e| e.to_string())?;
        Ok(Some(uris_to_paths(response.uris())))
    })
    .await
}

/// Turn portal URIs into local paths.
///
/// Anything without a local path — an `http:` handler, say — is dropped rather
/// than turned into a bogus one, since a transfer needs a real file.
fn uris_to_paths(uris: &[ashpd::Uri]) -> Vec<PathBuf> {
    uris.iter()
        .filter_map(|uri| file_uri_to_path(uri.as_str()))
        .collect()
}

/// Parse a `file://` URI into a path.
///
/// `ashpd::Uri` only exposes its string form, and a filename may legitimately
/// contain spaces and other characters the URI escaped, so the percent-decoding
/// happens here. An empty authority is the local machine, which is what the
/// portal always returns; a non-empty one is not a local file and is refused.
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    // `file:///path` has an empty authority; `file://host/path` does not.
    let path = uri.strip_prefix("file://")?;
    if !path.starts_with('/') {
        return None;
    }
    Some(PathBuf::from(percent_decode(path)))
}

/// Decode `%XX` escapes in a URI path.
///
/// A malformed escape is left as written rather than treated as an error: a
/// filename containing a literal `%` should not make the whole pick fail.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // A portal URI for a real filename is always valid UTF-8 once decoded.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// The value of one hex digit, or `None` if it is not one.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_become_local_paths() {
        let uris: Vec<ashpd::Uri> = vec![
            "file:///home/me/a.txt".parse().expect("a valid uri"),
            "file:///home/me/b%20c.txt".parse().expect("a valid uri"),
        ];
        assert_eq!(
            uris_to_paths(&uris),
            vec![
                PathBuf::from("/home/me/a.txt"),
                PathBuf::from("/home/me/b c.txt"),
            ],
            "percent-escapes are decoded"
        );
    }

    #[test]
    fn non_file_and_remote_uris_are_dropped() {
        let uris: Vec<ashpd::Uri> = vec![
            "https://example.com/a.txt".parse().expect("a valid uri"),
            "file://server/share/a.txt".parse().expect("a valid uri"),
            "file:///home/me/a.txt".parse().expect("a valid uri"),
        ];
        assert_eq!(
            uris_to_paths(&uris),
            vec![PathBuf::from("/home/me/a.txt")],
            "only local paths are usable for a transfer"
        );
    }

    #[test]
    fn an_empty_response_is_an_empty_selection() {
        assert!(uris_to_paths(&[]).is_empty());
    }

    #[test]
    fn unicode_and_escaped_percent_survive() {
        assert_eq!(
            file_uri_to_path("file:///home/me/caf%C3%A9.txt"),
            Some(PathBuf::from("/home/me/caf\u{e9}.txt")),
            "a multi-byte escape decodes to one character"
        );
        assert_eq!(
            file_uri_to_path("file:///home/me/100%25.txt"),
            Some(PathBuf::from("/home/me/100%.txt")),
            "an escaped percent sign is literal"
        );
    }

    #[test]
    fn a_malformed_escape_is_left_as_written() {
        // A filename may genuinely contain a percent sign.
        for (uri, expected) in [
            ("file:///home/me/50%.txt", "/home/me/50%.txt"),
            ("file:///home/me/%zz.txt", "/home/me/%zz.txt"),
            ("file:///home/me/trailing%2", "/home/me/trailing%2"),
        ] {
            assert_eq!(
                file_uri_to_path(uri),
                Some(PathBuf::from(expected)),
                "{uri}"
            );
        }
    }

    #[test]
    fn only_absolute_file_uris_are_accepted() {
        for bad in [
            "file://",
            "file://host",
            "file://host/share/a.txt",
            "/home/me/a.txt",
            "",
            "file:relative",
        ] {
            assert_eq!(file_uri_to_path(bad), None, "{bad:?} should be refused");
        }
    }
}
