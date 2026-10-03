//! Embedded icon and artwork assets.
//!
//! GPUI ships no icon set, so the GTK build's Adwaita names now map to a set of
//! vendored Lucide SVGs on a 24x24 grid with a 2px stroke, baked in with
//! `include_bytes!`. The semantic adbshare filenames are kept so call sites still
//! read as intent — `folder-download.svg` is Lucide's `folder-down` — and
//! `tools/fetch_icons.py` refreshes the set from a Lucide checkout. That drops
//! the GTK build's habit of writing assets to a temp directory at startup,
//! because GPUI resolves them through an in-process [`AssetSource`] instead.
//!
//! Two kinds of asset live here:
//!
//! * **chrome icons** (`icons/*.svg`) are monochrome and drawn with `currentColor`,
//!   so they are rendered with [`gpui::svg`] and tinted per call site.
//! * **artwork** (`art/*.svg`) is full-colour and is rendered with [`gpui::img`],
//!   which rasterises SVG without the alpha-mask tinting that `svg()` applies.
//!
//! Lucide is ISC-licensed and its notice has to travel with the files, so it is
//! both vendored at `assets/icons/LICENSE` and compiled into the binary as
//! [`LUCIDE_LICENSE`], which the About dialog shows. Shipping the SVGs without
//! the notice would not satisfy the licence.

use std::borrow::Cow;

use gpui::prelude::*;
use gpui::{AssetSource, ImageSource, Img, Result, Rgba, SharedString, Svg, px, svg};

macro_rules! asset_table {
    ($($name:literal => $path:literal,)*) => {
        /// Every embedded asset, keyed by the path GPUI will ask for.
        pub const ASSETS: &[(&str, &[u8])] = &[
            $(($name, include_bytes!($path)),)*
        ];
    };
}

asset_table! {
    // ── chrome ──────────────────────────────────────────────────────────
    "icons/application-x-executable.svg" => "../assets/icons/application-x-executable.svg",
    "icons/audio-x-generic.svg" => "../assets/icons/audio-x-generic.svg",
    "icons/camera-photo.svg" => "../assets/icons/camera-photo.svg",
    "icons/checkbox-checked.svg" => "../assets/icons/checkbox-checked.svg",
    "icons/dialog-error.svg" => "../assets/icons/dialog-error.svg",
    "icons/dialog-information.svg" => "../assets/icons/dialog-information.svg",
    "icons/dialog-warning.svg" => "../assets/icons/dialog-warning.svg",
    "icons/document-edit.svg" => "../assets/icons/document-edit.svg",
    "icons/document-open.svg" => "../assets/icons/document-open.svg",
    "icons/document-send.svg" => "../assets/icons/document-send.svg",
    "icons/drive-harddisk.svg" => "../assets/icons/drive-harddisk.svg",
    "icons/edit-copy.svg" => "../assets/icons/edit-copy.svg",
    "icons/edit-delete.svg" => "../assets/icons/edit-delete.svg",
    "icons/edit-find.svg" => "../assets/icons/edit-find.svg",
    "icons/edit-paste.svg" => "../assets/icons/edit-paste.svg",
    "icons/edit-select-all.svg" => "../assets/icons/edit-select-all.svg",
    "icons/edit-undo.svg" => "../assets/icons/edit-undo.svg",
    "icons/emblem-symbolic-link.svg" => "../assets/icons/emblem-symbolic-link.svg",
    "icons/emblem-synchronizing.svg" => "../assets/icons/emblem-synchronizing.svg",
    "icons/folder.svg" => "../assets/icons/folder.svg",
    "icons/folder-copy.svg" => "../assets/icons/folder-copy.svg",
    "icons/folder-documents.svg" => "../assets/icons/folder-documents.svg",
    "icons/folder-download.svg" => "../assets/icons/folder-download.svg",
    "icons/folder-music.svg" => "../assets/icons/folder-music.svg",
    "icons/folder-new.svg" => "../assets/icons/folder-new.svg",
    "icons/folder-open.svg" => "../assets/icons/folder-open.svg",
    "icons/folder-pictures.svg" => "../assets/icons/folder-pictures.svg",
    "icons/folder-upload.svg" => "../assets/icons/folder-upload.svg",
    "icons/folder-videos.svg" => "../assets/icons/folder-videos.svg",
    "icons/go-down.svg" => "../assets/icons/go-down.svg",
    "icons/go-down-bold.svg" => "../assets/icons/go-down-bold.svg",
    "icons/go-next.svg" => "../assets/icons/go-next.svg",
    "icons/go-previous.svg" => "../assets/icons/go-previous.svg",
    "icons/go-up.svg" => "../assets/icons/go-up.svg",
    "icons/help-about.svg" => "../assets/icons/help-about.svg",
    "icons/image-x-generic.svg" => "../assets/icons/image-x-generic.svg",
    "icons/media-playback-pause.svg" => "../assets/icons/media-playback-pause.svg",
    "icons/media-playback-start.svg" => "../assets/icons/media-playback-start.svg",
    "icons/network-wireless.svg" => "../assets/icons/network-wireless.svg",
    "icons/object-select.svg" => "../assets/icons/object-select.svg",
    "icons/package-x-generic.svg" => "../assets/icons/package-x-generic.svg",
    "icons/phone.svg" => "../assets/icons/phone.svg",
    "icons/process-stop.svg" => "../assets/icons/process-stop.svg",
    "icons/send-to.svg" => "../assets/icons/send-to.svg",
    "icons/sidebar-show.svg" => "../assets/icons/sidebar-show.svg",
    "icons/system-file-manager.svg" => "../assets/icons/system-file-manager.svg",
    "icons/system-lock.svg" => "../assets/icons/system-lock.svg",
    "icons/system-software-install.svg" => "../assets/icons/system-software-install.svg",
    "icons/text-x-generic.svg" => "../assets/icons/text-x-generic.svg",
    "icons/user-home.svg" => "../assets/icons/user-home.svg",
    "icons/user-trash.svg" => "../assets/icons/user-trash.svg",
    "icons/utilities-terminal.svg" => "../assets/icons/utilities-terminal.svg",
    "icons/video-x-generic.svg" => "../assets/icons/video-x-generic.svg",
    "icons/view-continuous.svg" => "../assets/icons/view-continuous.svg",
    "icons/view-grid.svg" => "../assets/icons/view-grid.svg",
    "icons/view-list.svg" => "../assets/icons/view-list.svg",
    "icons/view-more.svg" => "../assets/icons/view-more.svg",
    "icons/view-refresh.svg" => "../assets/icons/view-refresh.svg",
    "icons/window-close.svg" => "../assets/icons/window-close.svg",
    "icons/x-office-document.svg" => "../assets/icons/x-office-document.svg",

    // ── full-colour file-type artwork ──────────────────────────────────
    "art/apk.svg" => "../assets/art/apk.svg",
    "art/documents.svg" => "../assets/art/documents.svg",
    "art/movie.svg" => "../assets/art/movie.svg",
    "art/podcasts.svg" => "../assets/art/podcasts.svg",
    "art/tar.svg" => "../assets/art/tar.svg",
    "art/txt.svg" => "../assets/art/txt.svg",
    "art/wallpaper.svg" => "../assets/art/wallpaper.svg",
    "art/zip.svg" => "../assets/art/zip.svg",
}

/// Lucide's ISC notice, compiled in so it travels with the SVGs embedded above.
///
/// ISC requires the notice to appear in copies of the licensed work, and these
/// files ship inside the binary; the same notice is installed by the release
/// tarball and the AUR package. The About dialog renders this.
pub const LUCIDE_LICENSE: &str = include_str!("../assets/icons/LICENSE");

/// Serves [`ASSETS`] to GPUI. Installed on the [`gpui::Application`] in `main`.
pub struct AdbShareAssets;

impl AssetSource for AdbShareAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

// ── Chrome icons ─────────────────────────────────────────────────────────────

/// The icon names available to [`icon`], without the `icons/` prefix.
///
/// Kept as an enum-free list so a typo in a call site is a `&'static str` that
/// simply renders nothing; `crate::ui::icon_button` is the intended entry point
/// and takes one of these.
/// The chrome icon names, as full asset keys.
///
/// These are the names the UI asks for, not the upstream filenames: a call site
/// reads `names::FOLDER_DOWNLOAD` because that is the intent, while the asset
/// behind it is Lucide's `folder-down`. `tools/fetch_icons.py` holds the mapping
/// and the reason for each one.
///
/// Holding the whole key rather than a bare name means [`icon`] can pass it
/// straight through. It used to `format!("icons/{name}.svg")`, which allocated a
/// String per icon per frame — a hundred or so allocations on every repaint.
pub mod names {
    /// Full-colour file-type artwork. Kept beside the chrome names so a call
    /// site's asset and its intent read together.
    pub mod art {
        pub const APK: &str = "art/apk.svg";
        pub const DOCUMENTS: &str = "art/documents.svg";
        pub const MOVIE: &str = "art/movie.svg";
        pub const PODCASTS: &str = "art/podcasts.svg";
        pub const TAR: &str = "art/tar.svg";
        pub const TXT: &str = "art/txt.svg";
        pub const WALLPAPER: &str = "art/wallpaper.svg";
        pub const ZIP: &str = "art/zip.svg";
    }

    pub const AUDIO_GENERIC: &str = "icons/audio-x-generic.svg";
    pub const CAMERA_PHOTO: &str = "icons/camera-photo.svg";
    pub const CHECKBOX_CHECKED: &str = "icons/checkbox-checked.svg";
    pub const DIALOG_ERROR: &str = "icons/dialog-error.svg";
    pub const DIALOG_INFORMATION: &str = "icons/dialog-information.svg";
    pub const DIALOG_WARNING: &str = "icons/dialog-warning.svg";
    pub const DOCUMENT: &str = "icons/x-office-document.svg";
    /// Lucide's `download`: an arrow dropping into a tray. The same glyph as
    /// [`SOFTWARE_INSTALL`], named for the transfer rather than the APK
    /// install, so a call site reads as what the button does.
    pub const DOWNLOAD: &str = "icons/system-software-install.svg";
    pub const DOCUMENT_EDIT: &str = "icons/document-edit.svg";
    pub const DOCUMENT_OPEN: &str = "icons/document-open.svg";
    pub const DRIVE_HARDDISK: &str = "icons/drive-harddisk.svg";
    pub const EDIT_COPY: &str = "icons/edit-copy.svg";
    pub const EDIT_DELETE: &str = "icons/edit-delete.svg";
    pub const EDIT_FIND: &str = "icons/edit-find.svg";
    pub const EDIT_PASTE: &str = "icons/edit-paste.svg";
    pub const EDIT_UNDO: &str = "icons/edit-undo.svg";
    pub const EMBLEM_SYNC: &str = "icons/emblem-synchronizing.svg";
    pub const FILE_MANAGER: &str = "icons/system-file-manager.svg";
    pub const FOLDER: &str = "icons/folder.svg";
    pub const FOLDER_DOCUMENTS: &str = "icons/folder-documents.svg";
    pub const FOLDER_DOWNLOAD: &str = "icons/folder-download.svg";
    pub const FOLDER_MUSIC: &str = "icons/folder-music.svg";
    pub const FOLDER_NEW: &str = "icons/folder-new.svg";
    pub const FOLDER_OPEN: &str = "icons/folder-open.svg";
    pub const FOLDER_PICTURES: &str = "icons/folder-pictures.svg";
    pub const FOLDER_UPLOAD: &str = "icons/folder-upload.svg";
    pub const FOLDER_VIDEOS: &str = "icons/folder-videos.svg";
    pub const GO_NEXT: &str = "icons/go-next.svg";
    pub const GO_PREVIOUS: &str = "icons/go-previous.svg";
    pub const GO_UP: &str = "icons/go-up.svg";
    pub const HELP_ABOUT: &str = "icons/help-about.svg";
    pub const HOME: &str = "icons/user-home.svg";
    pub const IMAGE_GENERIC: &str = "icons/image-x-generic.svg";
    pub const OBJECT_SELECT: &str = "icons/object-select.svg";
    pub const PACKAGE: &str = "icons/package-x-generic.svg";
    pub const PAUSE: &str = "icons/media-playback-pause.svg";
    pub const PHONE: &str = "icons/phone.svg";
    pub const PLAY: &str = "icons/media-playback-start.svg";
    pub const REFRESH: &str = "icons/view-refresh.svg";
    pub const SEND_TO: &str = "icons/send-to.svg";
    pub const SIDEBAR_SHOW: &str = "icons/sidebar-show.svg";
    pub const SOFTWARE_INSTALL: &str = "icons/system-software-install.svg";
    pub const STOP: &str = "icons/process-stop.svg";
    pub const TERMINAL: &str = "icons/utilities-terminal.svg";
    pub const TEXT_GENERIC: &str = "icons/text-x-generic.svg";
    pub const TRASH: &str = "icons/user-trash.svg";
    pub const VIDEO_GENERIC: &str = "icons/video-x-generic.svg";
    pub const VIEW_GRID: &str = "icons/view-grid.svg";
    pub const VIEW_LIST: &str = "icons/view-list.svg";
    pub const VIEW_MORE: &str = "icons/view-more.svg";
    pub const WIRELESS: &str = "icons/network-wireless.svg";
}

/// Render a monochrome chrome icon tinted to `color`.
///
/// `key` is one of the [`names`] constants, which already carries the
/// `icons/...` prefix. The icons are Lucide: drawn on a 24x24 grid with a 2px
/// stroke and round caps, and tinted here through the alpha mask, so one file
/// serves every colour the UI needs.
///
/// `size` is the square edge in logical pixels; the SVG rasterises at 2x for
/// crispness on HiDPI displays.
pub fn icon(key: &'static str, size: f32, color: Rgba) -> Svg {
    svg().path(key).size(px(size)).text_color(color)
}

/// Full-colour file-type artwork, sized to fit a square box.
///
/// Unlike the chrome icons these are not tinted: they are the design's own
/// illustrations for an APK, an archive, a video and so on, and are rendered
/// through `img` so their colour survives.
pub fn artwork(key: &'static str, size: f32) -> Img {
    gpui::img(ImageSource::from(SharedString::from(key)))
        .size(px(size))
        .object_fit(gpui::ObjectFit::Contain)
}
