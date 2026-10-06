//! Persisted view preferences.
//!
//! The GTK build kept the sidebar width, grid zoom, view mode and the
//! "show hidden files" toggle only for the life of the process, so every launch
//! started from the same defaults and the first thing a regular user did was
//! drag the divider and re-pick the layout. That is a small thing to notice once
//! and annoying forever, so they are written to a JSON file and read back at
//! startup.
//!
//! Every field is optional and every read is fallible: a missing, unreadable or
//! corrupt file simply means "use the defaults", never an error the user has to
//! deal with.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::browser::ViewMode;

/// Sidebar width bounds, matching the divider's clamp.
const SIDEBAR_MIN: f32 = 200.0;
const SIDEBAR_MAX: f32 = 300.0;
const SIDEBAR_DEFAULT: f32 = 232.0;
/// Zoom bounds, matching the browser's clamp.
const ZOOM_MIN: f32 = 24.0;
const ZOOM_MAX: f32 = 128.0;
pub const ZOOM_DEFAULT: f32 = 48.0;

/// What the user last chose. Field names are the on-disk format; add fields
/// freely, they default when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Sidebar width in logical pixels.
    pub sidebar_width: f32,
    /// Whether the sidebar starts visible.
    pub sidebar_visible: bool,
    /// Grid icon size in logical pixels.
    pub zoom: f32,
    /// `true` for the list layout.
    pub list_view: bool,
    /// Whether dotfiles are shown.
    pub show_hidden: bool,
    /// What listings are ordered by.
    pub sort_key: crate::protocol::SortKey,
    /// Whether that order is reversed.
    pub sort_descending: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            sidebar_visible: true,
            zoom: ZOOM_DEFAULT,
            list_view: false,
            show_hidden: false,
            sort_key: crate::protocol::SortKey::default(),
            sort_descending: false,
        }
    }
}

impl Preferences {
    /// Clamp anything out of range, so a hand-edited or corrupted file cannot
    /// produce an unusable window.
    fn sanitised(mut self) -> Self {
        if !self.sidebar_width.is_finite() {
            self.sidebar_width = SIDEBAR_DEFAULT;
        }
        self.sidebar_width = self.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        if !self.zoom.is_finite() {
            self.zoom = ZOOM_DEFAULT;
        }
        self.zoom = self.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        self
    }

    /// Read the preferences, falling back to the defaults on any problem.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Self>(&text)
                .unwrap_or_default()
                .sanitised(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                tracing::warn!("could not read {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Write the preferences, ignoring failures.
    ///
    /// Losing a preference is not worth interrupting the user over, and this
    /// runs on the frame path, so it must never panic.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        let Some(parent) = path.parent() else { return };
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!("could not create {}: {err}", parent.display());
            return;
        }
        let text = match serde_json::to_string_pretty(self) {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!("could not serialise preferences: {err}");
                return;
            }
        };
        if let Err(err) = write_atomically(&path, text.as_bytes()) {
            tracing::warn!("could not write {}: {err}", path.display());
        }
    }

    pub fn view_mode(&self) -> ViewMode {
        if self.list_view {
            ViewMode::List
        } else {
            ViewMode::Grid
        }
    }

    /// Just the parts the browser cares about, so it can take them in one
    /// argument instead of five.
    pub fn view(&self) -> ViewPreferences {
        ViewPreferences {
            zoom: self.zoom,
            view_mode: self.view_mode(),
            show_hidden: self.show_hidden,
            sort_key: self.sort_key,
            sort_descending: self.sort_descending,
        }
    }
}

/// The browser's share of the preferences.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewPreferences {
    pub zoom: f32,
    pub view_mode: ViewMode,
    pub show_hidden: bool,
    pub sort_key: crate::protocol::SortKey,
    pub sort_descending: bool,
}

/// `~/.config/adbshare/gui.json`, or the XDG equivalent.
/// Write `bytes` to `path` so a reader never sees a half-written file.
///
/// `fs::write` truncates in place and then writes, so a crash, a full disk or a
/// kill between the two left the preferences truncated — usually empty, which
/// then failed to parse and silently reset every setting to its default. Write a
/// sibling temp file, flush it to disk, and `rename` it over the target: within
/// one directory the rename is atomic, so the file is either the old contents
/// or the new ones.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    // Same directory as the target, so the rename cannot cross a filesystem
    // (which would make it a copy, and non-atomic).
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);

    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // Without the flush the rename can land before the data does, which
        // defeats the point on a machine that loses power.
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    // Best effort: persist the directory entry too. Not all platforms allow
    // opening a directory for this, and failing to should not fail the save.
    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or(path)) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("adbshare").join("gui.json"))
}

/// The sidebar width bounds, shared with the app root that clamps the divider.
pub const SIDEBAR_RANGE: (f32, f32) = (SIDEBAR_MIN, SIDEBAR_MAX);
/// The grid zoom bounds, shared with the browser that clamps Ctrl+/-.
pub const ZOOM_RANGE: (f32, f32) = (ZOOM_MIN, ZOOM_MAX);
