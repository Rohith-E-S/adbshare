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
    crate::state::daemon::on_tokio(move || async move {
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
    crate::state::daemon::on_tokio(move || async move {
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
