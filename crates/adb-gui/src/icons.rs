//! Embedded icon and artwork assets.
//!
//! GPUI ships no icon set, so the GTK build's Adwaita names now map to Google's
//! **Material Symbols** in the Outlined style, baked in with `include_bytes!`.
//! The semantic adbshare filenames are kept so call sites still read as intent —
//! `folder-download.svg` is Material's `download` — and
//! `tools/fetch_icons.py` refreshes the set from the upstream repository. That
//! drops the GTK build's habit of writing assets to a temp directory at startup,
//! because GPUI resolves them through an in-process [`AssetSource`] instead.
//!
//! Material Symbols is Apache-2.0, which requires the notice to travel with the
//! files, so it is vendored alongside them at `assets/icons/NOTICE` and compiled
//! into the binary as [`MATERIAL_ATTRIBUTION`], which the About dialog shows.
//! The licence text itself is at `assets/icons/LICENSE`.
//!
//! Two kinds of asset live here:
//!
//! * **chrome icons** (`icons/*.svg`) are single-path outlines and are drawn
//!   with [`gpui::svg`] and tinted per call site — see [`hue`] for which colour
//!   each one wears.
//! * **artwork** (`art/*.png`, `folders/*.png`) is full-colour and is rendered
//!   with [`gpui::img`], which keeps the pixels instead of reducing the drawing
//!   to a tinted alpha mask. Both are PNGs rather than SVG for exactly that
//!   reason: they are filled, so `svg()` would flatten them, and `img()` cannot
//!   decode an SVG at all.
//!
//! Use [`icon_or_art`] wherever a key could be either, since handing a PNG to
//! [`icon`] draws a blank square.
//!
//! The folder PNGs are the Yaru icon theme's, which is CC-BY-SA-4.0 rather than
//! anything in this project's GPL licence. They are aggregated alongside the
//! code, unmodified, and attributed in the About dialog via
//! [`YARU_ATTRIBUTION`]; `assets/folders/` holds the notice and the licence.

use std::borrow::Cow;

use gpui::prelude::*;
use gpui::{AnyElement, AssetSource, ImageSource, Img, Result, Rgba, SharedString, Svg, px, svg};

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
    "icons/audio-x-generic.svg" => "../assets/icons/audio-x-generic.svg",
    "icons/camera-photo.svg" => "../assets/icons/camera-photo.svg",
    "icons/checkbox-checked.svg" => "../assets/icons/checkbox-checked.svg",
    "icons/dialog-error.svg" => "../assets/icons/dialog-error.svg",
    "icons/dialog-information.svg" => "../assets/icons/dialog-information.svg",
    "icons/dialog-warning.svg" => "../assets/icons/dialog-warning.svg",
    "icons/document-edit.svg" => "../assets/icons/document-edit.svg",
    "icons/document-open.svg" => "../assets/icons/document-open.svg",
    "icons/drive-harddisk.svg" => "../assets/icons/drive-harddisk.svg",
    "icons/edit-copy.svg" => "../assets/icons/edit-copy.svg",
    "icons/edit-delete.svg" => "../assets/icons/edit-delete.svg",
    "icons/edit-find.svg" => "../assets/icons/edit-find.svg",
    "icons/edit-paste.svg" => "../assets/icons/edit-paste.svg",
    "icons/edit-undo.svg" => "../assets/icons/edit-undo.svg",
    "icons/emblem-synchronizing.svg" => "../assets/icons/emblem-synchronizing.svg",
    "icons/folder-new.svg" => "../assets/icons/folder-new.svg",
    "icons/folder-upload.svg" => "../assets/icons/folder-upload.svg",
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
    "icons/system-software-install.svg" => "../assets/icons/system-software-install.svg",
    "icons/text-x-generic.svg" => "../assets/icons/text-x-generic.svg",
    "icons/user-home.svg" => "../assets/icons/user-home.svg",
    "icons/user-trash.svg" => "../assets/icons/user-trash.svg",
    "icons/utilities-terminal.svg" => "../assets/icons/utilities-terminal.svg",
    "icons/video-x-generic.svg" => "../assets/icons/video-x-generic.svg",
    "icons/view-grid.svg" => "../assets/icons/view-grid.svg",
    "icons/view-list.svg" => "../assets/icons/view-list.svg",
    "icons/view-more.svg" => "../assets/icons/view-more.svg",
    "icons/view-refresh.svg" => "../assets/icons/view-refresh.svg",
    "icons/x-office-document.svg" => "../assets/icons/x-office-document.svg",

    // ── full-colour folders ─────────────────────────────────────────────
    // PNGs, not SVGs, and that is not a preference. `svg()` renders through
    // usvg and then keeps only the alpha channel, tinting the silhouette with
    // one colour — so a filled purple folder drawn as SVG would come out as a
    // flat blob. `img()` keeps the pixels, which is what these need.
    "folders/folder.png" => "../assets/folders/folder.png",
    "folders/folder-documents.png" => "../assets/folders/folder-documents.png",
    "folders/folder-download.png" => "../assets/folders/folder-download.png",
    "folders/folder-music.png" => "../assets/folders/folder-music.png",
    "folders/folder-open.png" => "../assets/folders/folder-open.png",
    "folders/folder-pictures.png" => "../assets/folders/folder-pictures.png",
    "folders/folder-videos.png" => "../assets/folders/folder-videos.png",

    // ── full-colour file-type artwork ──────────────────────────────────
    // PNGs, for the same reason as the folders: `img()` decodes through the
    // `image` crate, which cannot read an SVG at all. These were Adwaita's own
    // SVGs, so they are rasterised from source rather than drawn from scratch.
    "art/apk.png" => "../assets/art/apk.png",
    "art/documents.png" => "../assets/art/documents.png",
    "art/movie.png" => "../assets/art/movie.png",
    "art/podcasts.png" => "../assets/art/podcasts.png",
    "art/tar.png" => "../assets/art/tar.png",
    "art/txt.png" => "../assets/art/txt.png",
    "art/wallpaper.png" => "../assets/art/wallpaper.png",
    "art/zip.png" => "../assets/art/zip.png",
}

/// Google's attribution for the embedded Material Symbols.
///
/// Apache-2.0 asks for the credit to accompany the work, and §4(d) asks that a
/// NOTICE file be carried with it — so the notice is vendored next to the SVGs
/// as `assets/icons/NOTICE` and compiled in here, which is what the About dialog
/// renders. A binary embedding these SVGs is a copy of them. The licence text
/// itself is at `assets/icons/LICENSE`.
pub const MATERIAL_ATTRIBUTION: &str = include_str!("../assets/icons/NOTICE");

/// Attribution for the embedded Yaru folder PNGs.
///
/// CC-BY-SA-4.0 requires the credit to accompany the work, not the whole legal
/// text, so this is the notice and `assets/folders/LICENSE` holds the licence
/// itself. A binary embedding these PNGs is a copy of them.
pub const YARU_ATTRIBUTION: &str = "\
Folder icons from the Yaru icon theme (https://github.com/ubuntu/yaru), \
copyright 2018 Sam Hewitt, derived from Adwaita (Red Hat, Inc. and Canonical \
Ltd). Used under CC-BY-SA-4.0; the licence is in assets/folders/LICENSE.";

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
/// simply renders nothing; `crate::views::ui::icon_button` is the intended entry point
/// and takes one of these.
/// The chrome icon names, as full asset keys.
///
/// These are the names the UI asks for, not the upstream filenames: a call site
/// reads `names::FOLDER_UPLOAD` because that is the intent, while the asset
/// behind it is Material's `upload`. `tools/fetch_icons.py` holds the mapping
/// and the reason for each one.
///
/// Holding the whole key rather than a bare name means [`icon`] can pass it
/// straight through. It used to `format!("icons/{name}.svg")`, which allocated a
/// String per icon per frame — a hundred or so allocations on every repaint.
pub mod names {
    /// Full-colour file-type artwork. Kept beside the chrome names so a call
    /// site's asset and its intent read together.
    ///
    /// Artwork is only ever reached through [`icon_or_art`], which sends a PNG
    /// down the pixel-preserving path; a `.svg` key here would be handed to
    /// `img()`, which cannot decode one, and would silently draw nothing.
    pub mod art {
        pub const APK: &str = "art/apk.png";
        pub const DOCUMENTS: &str = "art/documents.png";
        pub const MOVIE: &str = "art/movie.png";
        pub const PODCASTS: &str = "art/podcasts.png";
        pub const TAR: &str = "art/tar.png";
        pub const TXT: &str = "art/txt.png";
        pub const WALLPAPER: &str = "art/wallpaper.png";
        pub const ZIP: &str = "art/zip.png";
    }

    /// Full-colour folder artwork, drawn through [`artwork`] rather than
    /// [`icon`]. These are the same Yaru folders the desktop shows, so a
    /// directory looks in this window like it looks everywhere else on the
    /// system. `folder_icon_for` picks between them by name.
    pub mod folders {
        pub const FOLDER: &str = "folders/folder.png";
        pub const DOCUMENTS: &str = "folders/folder-documents.png";
        pub const DOWNLOAD: &str = "folders/folder-download.png";
        pub const MUSIC: &str = "folders/folder-music.png";
        pub const OPEN: &str = "folders/folder-open.png";
        pub const PICTURES: &str = "folders/folder-pictures.png";
        pub const VIDEOS: &str = "folders/folder-videos.png";
    }

    pub const AUDIO_GENERIC: &str = "icons/audio-x-generic.svg";
    pub const CAMERA_PHOTO: &str = "icons/camera-photo.svg";
    pub const CHECKBOX_CHECKED: &str = "icons/checkbox-checked.svg";
    pub const DIALOG_ERROR: &str = "icons/dialog-error.svg";
    pub const DIALOG_INFORMATION: &str = "icons/dialog-information.svg";
    pub const DIALOG_WARNING: &str = "icons/dialog-warning.svg";
    pub const DOCUMENT: &str = "icons/x-office-document.svg";
    /// Material's `download`: an arrow dropping into a tray. The same glyph as
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
    pub const FOLDER_NEW: &str = "icons/folder-new.svg";
    pub const FOLDER_UPLOAD: &str = "icons/folder-upload.svg";
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
/// `icons/...` prefix. The icons are Material Symbols Outlined: a single path
/// on a 24x24 grid, tinted here through the alpha mask, so one file serves every
/// colour the UI needs.
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

/// Draw an icon by asset key, picking the right pipeline for the file.
///
/// `icon()` throws away an SVG's colour and tints its silhouette, which is right
/// for line art and wrong for the filled folder PNGs, so those need `artwork()`.
/// Every call site that can receive either goes through here, because handing a
/// PNG to `icon()` renders a blank square rather than failing.
pub fn icon_or_art(key: &'static str, size: f32, color: Rgba) -> AnyElement {
    if key.ends_with(".png") {
        artwork(key, size).into_any_element()
    } else {
        icon(key, size, color).into_any_element()
    }
}

// ── Icon hues ─────────────────────────────────────────────────────────────────
//
// Material Symbols are single-path outlines, tinted through that path's alpha,
// so an icon has
// no colour of its own until something gives it one. Rather than let every call
// site reach for the palette's muted ink — which is what made every icon in the
// UI the same grey — each icon is given the hue its meaning suggests and the
// call site asks for the icon by name.
//
// The hues are a closed set rather than a colour per icon, so the UI reads as
// one system: six families from the app's theme, plus `Neutral` for the
// direction and editing verbs that would only be noise if they were coloured.
// [`crate::theme::Palette::hue`] turns one into a colour.

/// What an icon means, which is what decides its colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconHue {
    /// Anything that acts: refresh, send, open, info.
    Accent,
    /// Radios and storage: Wi-Fi, the terminal, a disk.
    Cyan,
    /// Motion and arrival: play, install, a download landing.
    Success,
    /// Things being made or moved: a new folder, a package, a warning.
    Warning,
    /// Anything destructive or stopped: trash, delete, close.
    Danger,
    /// Places and pictures: home, a photo, a sound file.
    Violet,
    /// Verbs with no colour of their own: back, forward, copy, paste, undo.
    Neutral,
}

/// The hue a chrome icon wears, from the icon's own asset key.
///
/// Keys with no meaning of their own — the full-colour artwork, which is not
/// tinted at all — come back [`IconHue::Accent`] and are never asked.
pub fn hue(key: &str) -> IconHue {
    match key {
        // Acts
        names::REFRESH
        | names::SEND_TO
        | names::DOCUMENT
        | names::DOCUMENT_OPEN
        | names::EMBLEM_SYNC
        | names::FILE_MANAGER
        | names::HELP_ABOUT
        | names::OBJECT_SELECT
        | names::CHECKBOX_CHECKED
        | names::DIALOG_INFORMATION => IconHue::Accent,

        // Radios, storage and the shell
        names::WIRELESS | names::DRIVE_HARDDISK | names::TERMINAL => IconHue::Cyan,

        // Motion, arrival, and the phone those transfers are for. `DOWNLOAD` is
        // deliberately absent: it is the same asset as `SOFTWARE_INSTALL`, so
        // listing both would make this arm unreachable.
        names::PLAY | names::SOFTWARE_INSTALL | names::FOLDER_UPLOAD | names::PHONE => {
            IconHue::Success
        }

        // Things being made, moved or flagged
        names::PAUSE
        | names::FOLDER_NEW
        | names::PACKAGE
        | names::DOCUMENT_EDIT
        | names::DIALOG_WARNING => IconHue::Warning,

        // Anything destructive or stopped. `VIDEO_GENERIC` is here because a
        // video reads as red the way it does on every desktop.
        names::TRASH
        | names::EDIT_DELETE
        | names::STOP
        | names::VIDEO_GENERIC
        | names::DIALOG_ERROR => IconHue::Danger,

        // Places, pictures and sound
        names::HOME | names::IMAGE_GENERIC | names::AUDIO_GENERIC | names::CAMERA_PHOTO => {
            IconHue::Violet
        }

        // Direction and editing verbs, the window controls and the grid/list
        // switch — which has to read as a matched pair, so both halves share a
        // hue rather than each picking one. A file's own name is on the row
        // beside it, so `TEXT_GENERIC` needs no colour of its own either.
        names::GO_NEXT
        | names::GO_PREVIOUS
        | names::GO_UP
        | names::VIEW_GRID
        | names::VIEW_LIST
        | names::VIEW_MORE
        | names::SIDEBAR_SHOW
        | names::EDIT_COPY
        | names::EDIT_FIND
        | names::EDIT_PASTE
        | names::EDIT_UNDO
        | names::TEXT_GENERIC => IconHue::Neutral,

        // Anything this module does not know a name for, which in practice
        // means full-colour artwork — never tinted, so never asked.
        _ => IconHue::Neutral,
    }
}
