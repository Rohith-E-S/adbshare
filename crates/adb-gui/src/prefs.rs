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
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(err) = std::fs::write(&path, text) {
                    tracing::warn!("could not write {}: {err}", path.display());
                }
            }
            Err(err) => tracing::warn!("could not serialise preferences: {err}"),
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
fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("adbshare").join("gui.json"))
}

/// The sidebar width bounds, shared with the app root that clamps the divider.
pub const SIDEBAR_RANGE: (f32, f32) = (SIDEBAR_MIN, SIDEBAR_MAX);
/// The grid zoom bounds, shared with the browser that clamps Ctrl+/-.
pub const ZOOM_RANGE: (f32, f32) = (ZOOM_MIN, ZOOM_MAX);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_within_bounds() {
        let p = Preferences::default();
        let (min, max) = SIDEBAR_RANGE;
        assert!((min..=max).contains(&p.sidebar_width));
        let (zmin, zmax) = ZOOM_RANGE;
        assert!((zmin..=zmax).contains(&p.zoom));
        assert!(!p.list_view);
        assert!(!p.show_hidden);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let p = Preferences {
            sidebar_width: 9_000.0,
            zoom: 1.0,
            ..Default::default()
        }
        .sanitised();
        assert_eq!(p.sidebar_width, SIDEBAR_MAX);
        assert_eq!(p.zoom, ZOOM_MIN);

        let p = Preferences {
            sidebar_width: -5.0,
            zoom: 9_000.0,
            ..Default::default()
        }
        .sanitised();
        assert_eq!(p.sidebar_width, SIDEBAR_MIN);
        assert_eq!(p.zoom, ZOOM_MAX);
    }

    #[test]
    fn non_finite_values_fall_back_to_the_defaults() {
        let p = Preferences {
            sidebar_width: f32::NAN,
            zoom: f32::INFINITY,
            ..Default::default()
        }
        .sanitised();
        assert_eq!(p.sidebar_width, SIDEBAR_DEFAULT);
        assert_eq!(p.zoom, ZOOM_DEFAULT);
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        // A file written by an older version, or a hand-trimmed one.
        let p: Preferences = serde_json::from_str(r#"{"list_view": true}"#).expect("partial file");
        assert!(p.list_view);
        assert_eq!(p.sidebar_width, SIDEBAR_DEFAULT);
        assert_eq!(p.zoom, ZOOM_DEFAULT);
    }

    #[test]
    fn a_corrupt_file_is_rejected_so_the_caller_can_fall_back() {
        for junk in ["not json", "", "[1, 2, 3]", r#"{"sidebar_width": "wide"}"#] {
            assert!(
                serde_json::from_str::<Preferences>(junk).is_err(),
                "{junk:?} should not parse"
            );
        }
    }

    #[test]
    fn the_sort_choice_survives_a_write_and_read() {
        let saved = Preferences {
            sort_key: crate::protocol::SortKey::Size,
            sort_descending: true,
            ..Default::default()
        };
        let text = serde_json::to_string(&saved).expect("serialises");
        let back: Preferences = serde_json::from_str(&text).expect("parses");
        assert_eq!(back.sort_key, crate::protocol::SortKey::Size);
        assert!(back.sort_descending);
        assert_eq!(back.view().sort_key, crate::protocol::SortKey::Size);
        assert!(back.view().sort_descending);
    }

    #[test]
    fn a_file_with_an_unknown_sort_key_falls_back_to_name() {
        let p: Preferences =
            serde_json::from_str(r#"{"sort_key": "by-colour", "sort_descending": true}"#)
                .expect("unknown keys are ignored, not fatal");
        assert_eq!(p.sort_key, crate::protocol::SortKey::Name);
        assert!(p.sort_descending, "the rest of the file still applies");
    }

    #[test]
    fn view_mode_round_trips_through_the_flag() {
        assert_eq!(Preferences::default().view_mode(), ViewMode::Grid);
        let p = Preferences {
            list_view: true,
            ..Default::default()
        };
        assert_eq!(p.view_mode(), ViewMode::List);
    }

    #[test]
    fn preferences_survive_a_write_and_read() {
        let dir = std::env::temp_dir().join(format!(
            "adbshare-prefs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui.json");

        let saved = Preferences {
            sidebar_width: 260.0,
            sidebar_visible: false,
            zoom: 72.0,
            list_view: true,
            show_hidden: true,
            sort_key: crate::protocol::SortKey::Modified,
            sort_descending: true,
        };
        std::fs::write(&path, serde_json::to_string_pretty(&saved).unwrap()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let loaded = serde_json::from_str::<Preferences>(&text)
            .expect("reads back")
            .sanitised();
        assert_eq!(loaded, saved);

        std::fs::remove_dir_all(&dir).ok();
    }
}
