//! The file browser: the pane that shows one directory of a phone or the disk.
//!
//! This is a GPUI entity rather than part of the app root because it owns a lot
//! of state the top bar does not care about — the current path, the listing, the
//! selection, scroll-back history, the view mode, search and zoom. It emits
//! [`BrowserEvent`] for anything that needs the daemon or the local filesystem;
//! the app root performs those operations and feeds results back in.
//!
//! That split is the direct translation of the GTK build, where `FileBrowser`
//! held the same state and called an `on_event` callback up into `app.rs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Div, Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Render, ScrollHandle, Size, Window, actions, div, px, relative, uniform_list,
};

use crate::icons::{self, names};
use crate::prefs;
use crate::protocol::{DirEntry, SortKey, contains_ignore_case, human_size, sort_entries};
use crate::theme::{self, Themed};
use crate::ui;

/// How entries are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    List,
}

/// How much one Ctrl+= / Ctrl+- press changes the icon size.
const ZOOM_STEP: f32 = 12.0;
/// Padding around a grid tile, on top of the icon.
const GRID_TILE_PAD: f32 = 14.0;

/// Height of the two lines under a grid tile's icon: the name and, for a file,
/// its size. Part of the tile's height, so it appears in both the tile and the
/// row maths and cannot drift between them.
const GRID_TILE_CAPTION: f32 = 34.0;
/// Gap between grid tiles.
const GRID_GAP: f32 = 8.0;
/// Height of one row in the list view.
const LIST_ROW_H: f32 = 30.0;
/// Height of the context strip above the file area: padding plus two text lines.
pub const CONTEXT_BAR_H: f32 = 38.0;
/// Height of the search strip, shown only while search is active.
pub const SEARCH_ROW_H: f32 = 40.0;

actions!(
    browser,
    [
        SelectAll,
        Deselect,
        CopySelection,
        Paste,
        Upload,
        Download,
        ToggleHidden,
        ZoomIn,
        ZoomOut,
        ToggleView,
        NewFolder,
        RenameFocused,
        DeleteSelection,
        DeletePermanently,
        Up,
        Back,
        Forward,
        Refresh,
        OpenFocused,
        ToggleSearch,
        TogglePathEntry,
        FocusPrevious,
        FocusNext,
        FocusFirst,
        FocusLast,
    ]
);

/// The key context the browser registers its actions under.
pub const BROWSER_CONTEXT: &str = "Browser";

/// One entry in a binding table: the key spelling, and a factory for the
/// `KeyBinding` it builds.
pub type Binding = (&'static str, fn() -> KeyBinding);

/// Every key binding the browser registers, as `(keys, factory)`.
///
/// Held as data rather than a bare `bind_keys` call so the shortcuts dialog can
/// be checked against what is actually bound. A test asserts the dialog claims
/// no key the browser does not bind — which is how the dialog came to promise
/// Alt+T for "open in terminal" when Alt+T actually switches the view.
///
/// The factory exists because each `actions!` entry is its own type, so the
/// bindings cannot be one homogeneous array.
pub const BINDINGS: &[Binding] = &[
    ("secondary-a", || {
        KeyBinding::new("secondary-a", SelectAll, None)
    }),
    ("delete", || {
        KeyBinding::new("delete", DeleteSelection, None)
    }),
    // Delete is recoverable on the disk and permanent on a device; Shift+Delete
    // is always permanent, which is what the context menu has always said.
    ("shift-delete", || {
        KeyBinding::new("shift-delete", DeletePermanently, None)
    }),
    ("f2", || KeyBinding::new("f2", RenameFocused, None)),
    // The dialog and the overflow menu both advertise this, so it has to exist.
    ("secondary-n", || {
        KeyBinding::new("secondary-n", NewFolder, None)
    }),
    ("alt-left", || KeyBinding::new("alt-left", Back, None)),
    ("alt-right", || KeyBinding::new("alt-right", Forward, None)),
    ("alt-up", || KeyBinding::new("alt-up", Up, None)),
    ("f5", || KeyBinding::new("f5", Refresh, None)),
    ("secondary-f", || {
        KeyBinding::new("secondary-f", ToggleSearch, None)
    }),
    ("secondary-l", || {
        KeyBinding::new("secondary-l", TogglePathEntry, None)
    }),
    // Enter opens, the way the context menu and the shortcuts dialog both say.
    // Text fields register their own `enter` under `TEXT_CONTEXT`, which takes
    // precedence while one has focus, so typing a path does not open a row.
    ("enter", || KeyBinding::new("enter", OpenFocused, None)),
    ("secondary-c", || {
        KeyBinding::new("secondary-c", CopySelection, None)
    }),
    ("secondary-v", || {
        KeyBinding::new("secondary-v", Paste, None)
    }),
    // Ctrl+U and Ctrl+Shift+C are upload and download, as every menu that
    // advertises them says. They used to be duplicate spellings of paste and
    // copy, which quietly turned two advertised shortcuts into the wrong action.
    ("secondary-u", || {
        KeyBinding::new("secondary-u", Upload, None)
    }),
    ("secondary-shift-c", || {
        KeyBinding::new("secondary-shift-c", Download, None)
    }),
    // Zoom, bound to the key names the platform actually reports rather than to
    // the obvious ones. They are not the X11 keysym names: pressing `-` on this
    // backend arrives as `key == "-"`, and the numeric keypad's `-` as
    // `key == "subtract"`; likewise `=` and `add`. So the bindings that read
    // "secondary-minus" and "secondary-equal" matched nothing at all, and Ctrl+plus
    // and Ctrl+minus did nothing on any keyboard.
    //
    // `secondary--` is not a typo. The binding parser reads a trailing `-` as a
    // literal `-` key, which is the only way to spell that key at all.
    ("secondary--", || {
        KeyBinding::new("secondary--", ZoomOut, None)
    }),
    ("secondary-subtract", || {
        KeyBinding::new("secondary-subtract", ZoomOut, None)
    }),
    ("secondary-=", || {
        KeyBinding::new("secondary-=", ZoomIn, None)
    }),
    ("secondary-add", || {
        KeyBinding::new("secondary-add", ZoomIn, None)
    }),
    // Ctrl+Shift+= is how a US layout types Ctrl+ +, and it arrives as `=` with
    // shift held. Modifiers have to match exactly, so it needs its own binding.
    ("secondary-shift-=", || {
        KeyBinding::new("secondary-shift-=", ZoomIn, None)
    }),
    ("alt-t", || KeyBinding::new("alt-t", ToggleView, None)),
    ("up", || KeyBinding::new("up", FocusPrevious, None)),
    ("down", || KeyBinding::new("down", FocusNext, None)),
    ("home", || KeyBinding::new("home", FocusFirst, None)),
    ("end", || KeyBinding::new("end", FocusLast, None)),
];

/// Register the browser's key bindings. Called once from `main`.
///
/// `secondary-` is Ctrl on Linux and Cmd on macOS; writing `cmd-` here would
/// bind the Super key, which is not what a file manager should use.
pub fn install_key_bindings(cx: &mut gpui::App) {
    cx.bind_keys(BINDINGS.iter().map(|(_, make)| make()));
}

/// Whether `keys` is bound by the browser or by the app root.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_bound(keys: &str) -> bool {
    BINDINGS.iter().any(|(bound, _)| *bound == keys)
        || crate::app::BINDINGS.iter().any(|(bound, _)| *bound == keys)
}

/// Anything the browser asks the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserEvent {
    /// Open a directory: follow it if it is one, download it otherwise.
    Open(DirEntry),
    /// Navigate to an absolute path, from a breadcrumb or the path bar.
    Navigate(PathBuf),
    /// Go to the parent directory.
    Up,
    Refresh,
    /// Create a directory with this name here.
    NewFolder(String),
    Rename(DirEntry, String),
    /// Delete the given entries, via the trash on the disk.
    Trash(Vec<DirEntry>),
    /// Delete the given entries permanently, the only option on a device.
    DeletePermanently(Vec<DirEntry>),
    Copy(Vec<DirEntry>),
    Paste,
    /// Send local files to the phone. Only meaningful while browsing the disk,
    /// which is where the chooser is anchored.
    Upload,
    /// Save the selection to the computer.
    Download(Vec<DirEntry>),
    /// Install an `.apk` on the connected device.
    InstallApk(DirEntry),
    /// Pause or resume every active job.
    TogglePauseTransfers,
    /// Cancel every active job.
    CancelTransfers,
    /// The user right-clicked, so the app should show a context menu here.
    ContextMenu {
        position: Point<Pixels>,
        focused: Option<usize>,
    },
    /// A double click or Enter on a directory: a listing is needed for it.
    OpenedDirectory(PathBuf),
    /// The keyboard revealed the search or path field, which needs the caret.
    FocusSearch,
    FocusPathEntry,
    /// Files arrived from another application or the desktop.
    DroppedPaths(Vec<PathBuf>),
    /// An entry was dropped onto a directory row, to move it inside.
    DroppedOnFolder(DirEntry, Vec<PathBuf>),
    /// Entries were dragged within this window, to move them onto a folder row.
    DraggedOntoFolder(DirEntry, Vec<DirEntry>),
}

/// What opening an entry turns into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    /// Follow a directory.
    Directory(PathBuf),
    /// An `.apk`, which the app offers to install.
    InstallApk(DirEntry),
    /// Anything else, which the app saves to the computer.
    Download(DirEntry),
}

impl OpenOutcome {
    /// The event the app root handles for this outcome.
    pub fn into_event(self) -> BrowserEvent {
        match self {
            OpenOutcome::Directory(path) => BrowserEvent::OpenedDirectory(path),
            OpenOutcome::InstallApk(entry) => BrowserEvent::InstallApk(entry),
            OpenOutcome::Download(entry) => BrowserEvent::Open(entry),
        }
    }
}

/// How a click changes the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extend {
    /// Replace the selection with the clicked row.
    None,
    /// Add or remove just the clicked row.
    Toggle,
    /// Add everything between the anchor and the clicked row.
    Range,
}

/// Aggregate byte counts across the active transfers, so the status bar can show
/// a bar and a throughput the way the GTK build did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferProgress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
}

impl TransferProgress {
    /// Fraction complete, or zero when nothing has a known size.
    pub fn fraction(&self) -> f32 {
        if self.bytes_total == 0 {
            return 0.0;
        }
        ((self.bytes_done as f64) / (self.bytes_total as f64)).clamp(0.0, 1.0) as f32
    }
}

/// What an in-window drag is carrying.
/// GPUI types a drag by payload, so this is how the browser tells its own drags
/// apart from files coming in from the desktop. Dragging something you already
/// own means moving it, so a drop on a folder row moves; the GTK build drew the
/// same distinction from whether the drag started inside the window.
#[derive(Debug, Clone)]
pub struct DraggedEntries(pub Vec<DirEntry>);

/// The chip that follows the cursor during an in-window drag.
///
/// One entity is created per browser and re-labelled per drag, because
/// [`InteractiveElement::on_drag`] wants an entity but hands its constructor
/// only an `&mut App`, which cannot build one.
pub struct DragGhost {
    label: String,
}

impl DragGhost {
    fn describe(payload: &DraggedEntries) -> String {
        match payload.0.as_slice() {
            [] => String::new(),
            [one] => one.name.clone(),
            many => format!("{} items", many.len()),
        }
    }
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *cx.theme();
        div()
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(t.capsule_border)
            .bg(t.capsule_bg)
            .text_size(px(11.0))
            .text_color(t.text_primary)
            .child(self.label.clone())
            .into_any_element()
    }
}

/// Geometry of the rendered item area, recorded during `render` so the
/// rubber-band selection can hit-test without asking the compositor.
#[derive(Debug, Clone, Copy, Default)]
pub struct GridGeometry {
    origin: Point<Pixels>,
    tile: Size<Pixels>,
    columns: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ListGeometry {
    origin_y: Pixels,
    row_h: f32,
}

/// The file browser pane.
pub struct Browser {
    // ── What is being browsed ──────────────────────────────────────────────
    /// The device serial, or [`crate::protocol::LOCAL_DEVICE`] for the disk.
    device: Option<String>,
    /// Friendly phone name, so breadcrumbs do not show a raw serial.
    device_display: String,
    current_path: PathBuf,
    /// True while showing the local disk rather than a device.
    local_mode: bool,
    /// The device's FUSE mountpoint, needed to open and preview device files.
    fuse_mount: Option<String>,

    // ── Contents ───────────────────────────────────────────────────────────
    entries: Vec<DirEntry>,
    /// Indices into `entries` that survive the hidden-file and search filters,
    /// in display order.
    visible: Vec<usize>,
    selection: BTreeSet<usize>,
    /// The row a context menu or rename acts on when nothing is selected.
    focused: Option<usize>,

    // ── Navigation history ─────────────────────────────────────────────────
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    /// Set while a Back or Forward navigation is in flight, so the abandoned
    /// path is not pushed back onto the opposite stack, which would make Back
    /// oscillate between two directories.
    navigating_history: bool,

    // ── Presentation ───────────────────────────────────────────────────────
    view_mode: ViewMode,
    show_hidden: bool,
    /// What listings are ordered by, and whether that order is reversed.
    sort_key: SortKey,
    sort_descending: bool,
    zoom: f32,
    search_query: String,
    search_active: bool,
    path_entry_active: bool,
    scroll: ScrollHandle,
    grid_geometry: GridGeometry,
    list_geometry: ListGeometry,
    /// The chip shown under the cursor during an in-window drag.
    drag_ghost: Entity<DragGhost>,
    /// Name of the directory row a drag is currently over, for the drop hint.
    hovered_drop_folder: Option<String>,

    // ── Item-area metrics ───────────────────────────────────────────────────
    //
    // `render` cannot ask the compositor for its own bounds, so the app root
    // reports where the file area is laid out. The rubber-band selection then
    // hit-tests against exact geometry instead of guessing.
    viewport_x: f32,
    viewport_y: f32,
    viewport_width: f32,
    viewport_height: f32,

    /// Tiles built by the last render, so the benchmarks can compare like with
    /// like.
    #[cfg(test)]
    last_built_tiles: usize,

    // ── Rubber-band selection ──────────────────────────────────────────────
    drag_anchor: Option<Point<Pixels>>,
    drag_current: Option<Point<Pixels>>,
    /// Whether the current drag extends the selection rather than replacing it.
    drag_extend: bool,

    // ── Status line ────────────────────────────────────────────────────────
    status_text: String,
    active_jobs: usize,
    /// Aggregate bytes moved by the active jobs, for the status bar's bar.
    progress: TransferProgress,
    /// True between asking for a listing and getting one, which is what greys
    /// the navigation buttons and puts "Loading…" in the status bar.
    loading: bool,
    /// Where a Shift-extended selection starts. A plain or Ctrl click sets it.
    extend_anchor: Option<usize>,
    transfers_paused: bool,

    focus: FocusHandle,
}

impl Browser {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            device: None,
            device_display: String::new(),
            current_path: PathBuf::from("/"),
            local_mode: false,
            fuse_mount: None,
            entries: Vec::new(),
            visible: Vec::new(),
            selection: BTreeSet::new(),
            focused: None,
            back: Vec::new(),
            forward: Vec::new(),
            navigating_history: false,
            view_mode: ViewMode::Grid,
            show_hidden: false,
            sort_key: SortKey::Name,
            sort_descending: false,
            zoom: prefs::ZOOM_DEFAULT,
            search_query: String::new(),
            search_active: false,
            path_entry_active: false,
            drag_ghost: cx.new(|_cx| DragGhost {
                label: String::new(),
            }),
            hovered_drop_folder: None,
            scroll: ScrollHandle::new(),
            grid_geometry: GridGeometry::default(),
            list_geometry: ListGeometry::default(),
            viewport_x: 0.0,
            viewport_y: 0.0,
            viewport_width: 0.0,
            viewport_height: 0.0,
            #[cfg(test)]
            last_built_tiles: 0,
            drag_anchor: None,
            drag_current: None,
            drag_extend: false,
            status_text: String::new(),
            active_jobs: 0,
            progress: TransferProgress::default(),
            loading: false,
            extend_anchor: None,
            transfers_paused: false,
            focus: cx.focus_handle(),
        }
    }

    // ── State the app root reads ────────────────────────────────────────────

    pub fn path(&self) -> &Path {
        &self.current_path
    }

    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    pub fn is_local(&self) -> bool {
        self.local_mode
    }

    /// True when there is anything to browse at all.
    pub fn has_device(&self) -> bool {
        self.device.is_some()
    }

    /// The name to show for the device in breadcrumbs.
    pub fn device_label(&self) -> String {
        if !self.device_display.is_empty() {
            return self.device_display.clone();
        }
        self.device
            .as_deref()
            .map(short_serial)
            .unwrap_or_else(|| "Phone".to_string())
    }

    /// The selected entries, in listing order.
    pub fn selected(&self) -> Vec<DirEntry> {
        self.selection
            .iter()
            .filter_map(|ix| self.entries.get(*ix))
            .cloned()
            .collect()
    }

    /// The entry a context menu or rename acts on.
    ///
    /// Falls back to the focused row, then the selection, so Rename and
    /// Properties always have a target.
    pub fn focused_entry(&self) -> Option<DirEntry> {
        self.focused
            .and_then(|ix| self.entries.get(ix))
            .cloned()
            .or_else(|| {
                self.selection
                    .iter()
                    .next()
                    .and_then(|ix| self.entries.get(*ix))
                    .cloned()
            })
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Whether there is a parent directory to go up to.
    pub fn can_go_up(&self) -> bool {
        self.current_path
            .parent()
            .is_some_and(|parent| parent != self.current_path)
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn search_active(&self) -> bool {
        self.search_active
    }

    pub fn path_entry_active(&self) -> bool {
        self.path_entry_active
    }

    /// The entry with this name, if the listing has it.
    pub fn entry_named(&self, name: &str) -> Option<&DirEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Apply the persisted view preferences.
    pub fn apply_preferences(
        &mut self,
        prefs: crate::prefs::ViewPreferences,
        cx: &mut Context<Self>,
    ) {
        self.zoom = step_zoom(prefs.zoom, 0.0);
        self.view_mode = prefs.view_mode;
        self.show_hidden = prefs.show_hidden;
        self.sort_key = prefs.sort_key;
        self.sort_descending = prefs.sort_descending;
        self.recompute_visible();
        cx.notify();
    }

    /// Select every visible row, as Ctrl+A does.
    pub fn select_all_entries(&mut self) {
        self.selection = self.visible.iter().copied().collect();
    }

    /// Show or hide dotfiles.
    pub fn set_show_hidden(&mut self, show: bool, cx: &mut Context<Self>) {
        self.show_hidden = show;
        self.recompute_visible();
        cx.notify();
    }

    /// Breadcrumb segments for the path bar, as `(label, path)`.
    ///
    /// The first segment names the source — the phone or "This computer" —
    /// rather than showing a bare `/`.
    pub fn breadcrumbs(&self) -> Vec<(String, PathBuf)> {
        let first_label = if self.local_mode {
            "This computer".to_string()
        } else {
            self.device_label()
        };
        let mut crumbs = vec![(first_label, PathBuf::from("/"))];
        if self.current_path == Path::new("/") {
            return crumbs;
        }

        let mut accum = PathBuf::from("/");
        for component in self.current_path.iter().filter(|c| *c != Path::new("/")) {
            accum.push(component);
            crumbs.push((component.to_string_lossy().to_string(), accum.clone()));
        }

        // `/sdcard` is where most browsing starts, so name it "Internal storage"
        // rather than leaving the raw path in the bar.
        if self.current_path.starts_with("/sdcard")
            && self.current_path != Path::new("/sdcard")
            && let Some(slot) = crumbs
                .iter_mut()
                .find(|(_, path)| path == &PathBuf::from("/sdcard"))
        {
            slot.0 = "Internal storage".to_string();
        }
        crumbs
    }

    /// Absolute path of an entry in the current directory.
    pub fn full_path(&self, entry: &DirEntry) -> PathBuf {
        self.current_path.join(&entry.name)
    }

    /// Where an entry lives on the local disk, using the FUSE mount when the
    /// entry came from a device. `None` when there is no such local path.
    pub fn local_path_of(&self, entry: &DirEntry) -> Option<PathBuf> {
        if self.local_mode {
            return Some(self.full_path(entry));
        }
        let mount = self.fuse_mount.as_deref()?;
        // FUSE mirrors the device root under the mountpoint, so strip the
        // leading slash and join onto the mount.
        let relative = self.full_path(entry);
        let relative = relative.strip_prefix("/").ok()?;
        Some(Path::new(mount).join(relative))
    }

    /// The browsed directory as a path on this machine.
    ///
    /// On the disk that is the path itself. On a phone the FUSE mount stands in
    /// for the device root, so the browsed path is re-rooted onto the mount:
    /// pointing a file manager at `/sdcard/Download` would show the host's root,
    /// which is not what anyone means by "open this folder".
    pub fn local_current_dir(&self) -> Option<PathBuf> {
        if self.local_mode {
            return Some(self.current_path.clone());
        }
        let mount = self.fuse_mount.as_deref()?;
        let relative = self.current_path.strip_prefix("/").ok()?;
        Some(Path::new(mount).join(relative))
    }

    // ── Mutators the app root drives ────────────────────────────────────────

    /// Point the browser at a device, starting at its Download folder.
    pub fn set_device(&mut self, serial: &str, display: &str, cx: &mut Context<Self>) {
        self.device = Some(serial.to_string());
        self.device_display = display.to_string();
        self.local_mode = false;
        self.clear_lists();
        self.current_path = PathBuf::from("/sdcard/Download");
        self.back.clear();
        self.forward.clear();
        self.status_text = format!("Browsing {display}");
        cx.notify();
    }

    /// Point the browser at the local disk.
    pub fn set_local(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.device = Some(crate::protocol::LOCAL_DEVICE.to_string());
        self.device_display.clear();
        self.local_mode = true;
        self.fuse_mount = None;
        self.clear_lists();
        self.current_path = path.to_path_buf();
        self.back.clear();
        self.forward.clear();
        self.status_text = format!("Browsing {}", path.display());
        cx.notify();
    }

    /// No device and no local target: show the onboarding state.
    pub fn set_idle(&mut self, cx: &mut Context<Self>) {
        self.device = None;
        self.local_mode = false;
        self.fuse_mount = None;
        self.clear_lists();
        self.current_path = PathBuf::from("/");
        cx.notify();
    }

    /// Record the device's FUSE mountpoint, which external opens and previews
    /// need. `None` when FUSE is disabled.
    pub fn set_fuse_mount(&mut self, mount: Option<String>, cx: &mut Context<Self>) {
        self.fuse_mount = mount;
        cx.notify();
    }

    /// Report where the file area is laid out, so rubber-band hit-testing has
    /// exact geometry to work from.
    /// What listings are currently ordered by.
    pub fn sort_key(&self) -> SortKey {
        self.sort_key
    }

    /// Whether the current order is reversed.
    pub fn sort_descending(&self) -> bool {
        self.sort_descending
    }

    /// Change how listings are ordered and re-sort what is on screen.
    pub fn set_sort(&mut self, key: SortKey, descending: bool, cx: &mut Context<Self>) {
        self.sort_key = key;
        self.sort_descending = descending;
        self.apply_sort(cx);
    }

    /// Re-sort the current listing in place.
    ///
    /// The daemon returns device listings folders-first by name, so the sort
    /// has to be re-applied here for the other keys to mean anything.
    pub fn apply_sort(&mut self, cx: &mut Context<Self>) {
        sort_entries(&mut self.entries, self.sort_key, self.sort_descending);
        cx.notify();
    }

    /// The current grid icon size.
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Whether this view currently holds key focus, and so whether the
    /// bindings in [`BINDINGS`] can fire at all.
    ///
    /// An action is dispatched along the focus path: GPUI starts at whatever
    /// holds key focus and walks *upwards* through its ancestors. The browser is
    /// a descendant of the app root, not an ancestor of it, so every binding
    /// here — zoom, the arrow keys, `Alt+T`, `Ctrl+F` — was registered and
    /// unreachable. With nothing focused at all, dispatch started at the root, so
    /// only the app's own handful of shortcuts ever worked.
    ///
    /// The fix is to hand the keyboard here rather than to duplicate every
    /// handler on the root: dispatch then starts here and continues up to the app
    /// root, so the app's Escape and `F9` still arrive.
    /// The width one grid tile is laid out at.
    ///
    /// The single source of truth for the tile's size: `grid_columns` sizes rows
    /// with it and `grid_geometry` hit-tests against it, so a test that wants to
    /// assert a tile came out the right size asks here rather than repeating the
    /// arithmetic.
    pub fn tile_width(&self) -> f32 {
        self.zoom + GRID_TILE_PAD * 2.0
    }

    pub fn has_key_focus(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    pub fn set_viewport(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        _cx: &mut Context<Self>,
    ) {
        self.viewport_x = x;
        self.viewport_y = y;
        self.viewport_width = width;
        self.viewport_height = height;
    }

    /// Install a directory listing, clearing any selection.
    pub fn set_entries(&mut self, entries: Vec<DirEntry>, cx: &mut Context<Self>) {
        self.install_entries(entries);
        cx.notify();
    }

    /// The state part of [`Self::set_entries`], so it can be driven without a
    /// `Context`.
    pub fn install_entries(&mut self, mut entries: Vec<DirEntry>) {
        self.loading = false;
        sort_entries(&mut entries, self.sort_key, self.sort_descending);
        self.entries = entries;
        self.selection.clear();
        self.focused = None;
        self.extend_anchor = None;
        self.recompute_visible();
    }

    /// Report a listing failure in the status bar.
    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.loading = false;
        self.entries.clear();
        self.selection.clear();
        self.visible.clear();
        self.extend_anchor = None;
        self.status_text = message;
        cx.notify();
    }

    /// Mark that a listing is on its way.
    ///
    /// Also the re-entrancy guard: a second request while one is in flight is
    /// dropped, because two answers for the same directory would race and the
    /// later one could be the staler of the pair.
    pub fn begin_load(&mut self, cx: &mut Context<Self>) -> bool {
        if self.loading {
            return false;
        }
        self.loading = true;
        cx.notify();
        true
    }

    /// Whether a listing is in flight.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Drop the in-flight guard without touching the listing on screen.
    ///
    /// Only [`Self::begin_load`]'s counterpart for a deliberate re-read: the
    /// stale answer is discarded by the path check in the fetch handler, so
    /// letting a second one start cannot show the wrong directory.
    pub fn cancel_load(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            self.loading = false;
            cx.notify();
        }
    }

    /// Update the transfer counters shown in the status bar.
    pub fn set_job_counts(
        &mut self,
        active: usize,
        total: usize,
        paused: bool,
        progress: TransferProgress,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self.active_jobs == active
            && self.transfers_paused == paused
            && self.progress.bytes_done == progress.bytes_done
            && self.progress.bytes_total == progress.bytes_total
            && self.progress.speed_bps == progress.speed_bps;
        self.active_jobs = active;
        self.transfers_paused = paused;
        self.progress = progress;
        if !unchanged {
            // Rewriting the label on every 600ms poll would make it flicker, so
            // it is only touched when the state actually changes.
            self.status_text = if total == 0 {
                "0 item(s)".to_string()
            } else {
                format!("{active} of {total} transfer(s) active")
            };
        }
        cx.notify();
    }

    /// Navigate to a path, recording where we came from.
    pub fn navigate(&mut self, path: PathBuf) {
        let leaving = self.current_path.clone();
        self.push_history(&leaving);
        self.current_path = path;
        self.clear_lists();
    }

    /// Go back one directory, moving the current path and the forward stack in
    /// the same step.
    ///
    /// Doing the move here rather than leaving it to the caller keeps the two
    /// stacks consistent: whichever history the user came from is exactly the
    /// one the other way can return along.
    pub fn go_back(&mut self) -> Option<PathBuf> {
        let target = self.back.pop()?;
        let leaving = std::mem::replace(&mut self.current_path, target.clone());
        self.forward.push(leaving);
        self.clear_lists();
        Some(target)
    }

    /// Go forward one directory, mirroring [`Self::go_back`].
    pub fn go_forward(&mut self) -> Option<PathBuf> {
        let target = self.forward.pop()?;
        let leaving = std::mem::replace(&mut self.current_path, target.clone());
        self.back.push(leaving);
        self.clear_lists();
        Some(target)
    }

    fn push_history(&mut self, leaving: &Path) {
        if self.navigating_history {
            self.navigating_history = false;
            return;
        }
        self.back.push(leaving.to_path_buf());
        // Moving somewhere new invalidates the forward stack, as in a browser.
        self.forward.clear();
    }

    fn clear_lists(&mut self) {
        self.entries.clear();
        self.visible.clear();
        self.selection.clear();
        self.focused = None;
        // The anchor is an index into the listing that just went away.
        self.extend_anchor = None;
    }

    // ── State transforms, kept free of `Context` so they can be tested ─────

    /// A plain click replaces the selection; Shift or Ctrl toggles the row in or
    /// out of it.
    pub fn click_row(&mut self, index: usize, extend: Extend) -> bool {
        match extend {
            // Ctrl toggles exactly one row.
            Extend::Toggle => {
                if !self.selection.insert(index) {
                    self.selection.remove(&index);
                }
                self.extend_anchor = Some(index);
            }
            // Shift extends from the anchor left by the last plain or Ctrl
            // click. GTK's ListBox and FlowBox gave ranges for free, so the
            // rewrite lost them by hand-rolling selection and nothing noticed
            // until the shortcuts dialog advertised "Shift or Ctrl + click".
            Extend::Range => match self.extend_anchor {
                Some(anchor) => self.extend_range(anchor, index),
                None => {
                    self.selection.clear();
                    self.selection.insert(index);
                    self.extend_anchor = Some(index);
                }
            },
            Extend::None => {
                self.selection.clear();
                self.selection.insert(index);
                self.extend_anchor = Some(index);
            }
        }
        self.focused = Some(index);
        true
    }

    /// Select everything between two positions in the visible order.
    ///
    /// Direction does not matter: dragging the band upwards is still a range
    /// between the same two rows.
    fn extend_range(&mut self, anchor: usize, index: usize) {
        let (Some(from), Some(to)) = (
            self.visible.iter().position(|ix| *ix == anchor),
            self.visible.iter().position(|ix| *ix == index),
        ) else {
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        // Shift extends the existing selection rather than replacing it, which
        // is what makes two disjoint bands possible.
        for slot in lo..=hi {
            self.selection.insert(self.visible[slot]);
        }
    }

    /// Move the focus, and with it the selection, for arrow-key traversal.
    pub fn focus_step(&mut self, delta: isize, extend: bool) -> bool {
        if self.visible.is_empty() {
            return false;
        }
        let current = self
            .focused
            .and_then(|ix| self.visible.iter().position(|v| *v == ix))
            .map(|pos| pos as isize)
            .unwrap_or(-1);
        let next = (current + delta).rem_euclid(self.visible.len() as isize) as usize;
        let index = self.visible[next];
        self.focused = Some(index);
        if !extend {
            self.selection.clear();
        }
        self.selection.insert(index);
        true
    }

    /// Apply the search filter and the hidden-file preference.
    fn recompute_visible(&mut self) {
        let needle = self.search_query.to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.show_hidden || !entry.name.starts_with('.'))
            .filter(|(_, entry)| contains_ignore_case(&entry.name, &needle))
            .map(|(ix, _)| ix)
            .collect();

        // Drop selections the filter just hid, so Copy and Delete cannot act on
        // something the user can no longer see. Membership goes through a set
        // rather than `visible.contains`: `visible` is sorted, but a linear
        // scan per selected row made this quadratic, which showed up as a
        // multi-second freeze on select-all followed by a search.
        let visible: std::collections::HashSet<usize> = self.visible.iter().copied().collect();
        self.selection.retain(|ix| visible.contains(ix));
        if self.focused.is_some_and(|ix| !visible.contains(&ix)) {
            self.focused = None;
        }
    }

    /// Set the search query from the search field.
    pub fn set_search_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.set_search_query_raw(query);
        cx.notify();
    }

    /// The state part of [`Self::set_search_query`].
    pub fn set_search_query_raw(&mut self, query: String) {
        self.search_query = query;
        self.recompute_visible();
    }

    /// Resolve a double click or Enter into what should happen.
    pub fn open_index(&mut self, index: usize) -> Option<OpenOutcome> {
        let entry = self.entries.get(index)?.clone();
        Some(if entry.looks_like_dir() {
            let target = self.full_path(&entry);
            let leaving = self.current_path.clone();
            self.push_history(&leaving);
            self.current_path = target.clone();
            self.clear_lists();
            OpenOutcome::Directory(target)
        } else if entry.ext() == "apk" {
            OpenOutcome::InstallApk(entry)
        } else {
            OpenOutcome::Download(entry)
        })
    }

    /// Whether pressing Delete is recoverable.
    ///
    /// The local disk has a trash; a device does not, so deletion there is
    /// always permanent and the caller must say so.
    pub fn delete_is_recoverable(&self) -> bool {
        self.local_mode
    }

    // ── Actions ────────────────────────────────────────────────────────────

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selection = self.visible.iter().copied().collect();
        cx.notify();
    }

    fn deselect(&mut self, _: &Deselect, _: &mut Window, cx: &mut Context<Self>) {
        self.deselect_action(cx);
    }

    fn deselect_action(&mut self, cx: &mut Context<Self>) {
        self.selection.clear();
        self.drag_anchor = None;
        self.drag_current = None;
        cx.notify();
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected();
        if !selected.is_empty() {
            cx.emit(BrowserEvent::Copy(selected));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(BrowserEvent::Paste);
    }

    fn upload(&mut self, _: &Upload, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(BrowserEvent::Upload);
    }

    fn download(&mut self, _: &Download, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected();
        if !selected.is_empty() {
            cx.emit(BrowserEvent::Download(selected));
        }
    }

    /// Clear the selection. Public so the app root can fall back to it once
    /// Escape has found no overlay to dismiss.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.deselect_action(cx);
    }

    fn toggle_hidden(&mut self, _: &ToggleHidden, _: &mut Window, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.recompute_visible();
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom = step_zoom(self.zoom, ZOOM_STEP);
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom = step_zoom(self.zoom, -ZOOM_STEP);
        cx.notify();
    }

    fn toggle_view(&mut self, _: &ToggleView, _: &mut Window, cx: &mut Context<Self>) {
        self.flip_view_mode(cx);
    }

    /// Flip between the grid and the list. See [`Self::step_back`] for why the
    /// top bar calls this directly.
    pub fn flip_view_mode(&mut self, cx: &mut Context<Self>) {
        self.view_mode = match self.view_mode {
            ViewMode::Grid => ViewMode::List,
            ViewMode::List => ViewMode::Grid,
        };
        cx.notify();
    }

    /// Switch to a specific layout, so each button sets what it names rather
    /// than toggling: the list button used to switch to the grid when pressed
    /// while already in the list.
    pub fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode != mode {
            self.view_mode = mode;
            cx.notify();
        }
    }

    /// Show or hide the search strip. See [`Self::step_back`] for why this is
    /// public.
    pub fn set_search_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.search_active == active {
            return;
        }
        self.search_active = active;
        if !active && !self.search_query.is_empty() {
            self.set_search_query_raw(String::new());
        }
        cx.notify();
    }

    fn new_folder(&mut self, _: &NewFolder, _: &mut Window, cx: &mut Context<Self>) {
        if self.has_device() {
            cx.emit(BrowserEvent::NewFolder(String::new()));
        }
    }

    fn rename_focused(&mut self, _: &RenameFocused, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.focused_entry() {
            let name = entry.name.clone();
            cx.emit(BrowserEvent::Rename(entry, name));
        }
    }

    fn delete_selection(&mut self, _: &DeleteSelection, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected();
        if selected.is_empty() {
            return;
        }
        if self.delete_is_recoverable() {
            cx.emit(BrowserEvent::Trash(selected));
        } else {
            cx.emit(BrowserEvent::DeletePermanently(selected));
        }
    }

    fn delete_permanently(
        &mut self,
        _: &DeletePermanently,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.selected();
        if !selected.is_empty() {
            cx.emit(BrowserEvent::DeletePermanently(selected));
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.step_up(cx);
    }

    /// Go to the parent directory. See [`Self::step_back`] for why this is public.
    pub fn step_up(&mut self, cx: &mut Context<Self>) {
        if self.can_go_up() {
            cx.emit(BrowserEvent::Up);
        }
    }

    fn back(&mut self, _: &Back, _: &mut Window, cx: &mut Context<Self>) {
        self.step_back(cx);
    }

    fn forward(&mut self, _: &Forward, _: &mut Window, cx: &mut Context<Self>) {
        self.step_forward(cx);
    }

    /// Go back one directory, asking the app for the listing.
    ///
    /// Public because the top bar's back button drives this directly. It used to
    /// dispatch the `Back` action instead, which travels the focus path — and
    /// nothing ever focuses the browser, so the button did nothing at all.
    pub fn step_back(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.go_back() {
            cx.notify();
            cx.emit(BrowserEvent::OpenedDirectory(target));
        }
    }

    /// Go forward one directory. See [`Self::step_back`] for why this is public.
    pub fn step_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.go_forward() {
            cx.notify();
            cx.emit(BrowserEvent::OpenedDirectory(target));
        }
    }

    fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.step_refresh(cx);
    }

    /// Ask for the listing again. See [`Self::step_back`] for why this is public.
    pub fn step_refresh(&mut self, cx: &mut Context<Self>) {
        cx.emit(BrowserEvent::Refresh);
    }

    fn open_focused(&mut self, _: &OpenFocused, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.focused_entry()
            && let Some(outcome) = self.open_index(self.index_of(&entry))
        {
            cx.emit(outcome.into_event());
        }
    }

    fn toggle_search(&mut self, _: &ToggleSearch, _: &mut Window, cx: &mut Context<Self>) {
        let next = !self.search_active;
        self.set_search_active(next, cx);
        if next {
            cx.emit(BrowserEvent::FocusSearch);
        }
    }

    fn focus_previous(&mut self, _: &FocusPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus_and_scroll(-1, false, window, cx);
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus_and_scroll(1, false, window, cx);
    }

    fn focus_first(&mut self, _: &FocusFirst, window: &mut Window, cx: &mut Context<Self>) {
        self.jump_focus(0, window, cx);
    }

    fn focus_last(&mut self, _: &FocusLast, window: &mut Window, cx: &mut Context<Self>) {
        self.jump_focus(-1, window, cx);
    }

    /// Move the focus one row and bring it into view.
    ///
    /// The list view is virtualised, so scrolling the row into view is what
    /// makes arrow-key navigation usable; the grid scrolls with the container.
    fn move_focus_and_scroll(
        &mut self,
        delta: isize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.focused;
        if !self.focus_step(delta, extend) {
            return;
        }
        if self.view_mode == ViewMode::List {
            self.scroll_to_focused();
        }
        let _ = (before, window);
        cx.notify();
    }

    /// Jump to the first or last row.
    fn jump_focus(&mut self, index: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let index: usize = if index < 0 {
            self.visible.len() - 1
        } else {
            (index as usize).min(self.visible.len() - 1)
        };
        let entry = self.visible[index];
        self.focused = Some(entry);
        self.selection.clear();
        self.selection.insert(entry);
        if self.view_mode == ViewMode::List {
            self.scroll.scroll_to_item(index);
        }
        let _ = window;
        cx.notify();
    }

    /// How far the item area starts below the top of the browser's own box.
    ///
    /// The context strip and, while it is showing, the search row are the
    /// browser's first children; the grid then adds its own padding. Anything
    /// that positions or hit-tests items has to start here, or drag-selection
    /// lands on the wrong row.
    pub fn item_area_top(&self) -> f32 {
        CONTEXT_BAR_H
            + if self.search_active {
                SEARCH_ROW_H
            } else {
                0.0
            }
            + GRID_TILE_PAD
    }

    /// How tall the item area is inside the browser's own box.
    pub fn item_area_height(&self) -> f32 {
        (self.viewport_height - self.item_area_top()).max(0.0)
    }

    /// How many tiles the grid built this frame, for the benchmarks.
    #[cfg(test)]
    pub fn built_tiles(&self) -> usize {
        self.last_built_tiles
    }

    /// Which grid rows to build this frame, and how many.
    ///
    /// Reads the scroll offset recorded by the last frame's layout, so the
    /// window is at most one frame stale. A row of overscan on each side keeps
    /// fast scrolling from showing gaps.
    fn visible_row_range(&self, tile_h: f32) -> (usize, usize) {
        const OVERSCAN: usize = 1;
        let viewport_h: f32 = self.item_area_height();
        if viewport_h <= 0.0 || tile_h <= 0.0 {
            // Before the first layout there is no viewport to measure, so fall
            // back to a modest window rather than building the whole folder.
            return (0, 24);
        }
        let scroll_y: f32 = self.scroll.offset().y.into();
        let visible = (viewport_h / tile_h).ceil() as usize + 1;
        let first = ((scroll_y / tile_h).floor() as isize - OVERSCAN as isize).max(0) as usize;
        (first, visible + OVERSCAN * 2 + 1)
    }

    /// Scroll the file area to a pixel offset.
    ///
    /// Only the windowing tests need this: the real UI scrolls through the
    /// pointer, and arrow-key navigation goes through the browser's own
    /// actions.
    #[cfg(test)]
    pub fn scroll_to(&self, y: f32) {
        self.scroll.set_offset(gpui::point(px(0.), px(y)));
    }

    /// Bring the focused row into view in the (virtualised) list view.
    fn scroll_to_focused(&self) {
        if let Some(slot) = self
            .focused
            .and_then(|entry| self.visible.iter().position(|v| *v == entry))
        {
            self.scroll.scroll_to_item(slot);
        }
    }

    fn toggle_path_entry(&mut self, _: &TogglePathEntry, _: &mut Window, cx: &mut Context<Self>) {
        self.path_entry_active = !self.path_entry_active;
        if self.path_entry_active {
            cx.emit(BrowserEvent::FocusPathEntry);
        }
        cx.notify();
    }

    /// Leave path-entry mode. Public so the app root's Escape handler can do it
    /// without going through the action, which would toggle rather than close.
    pub fn close_path_entry(&mut self, cx: &mut Context<Self>) {
        if self.path_entry_active {
            self.path_entry_active = false;
            cx.notify();
        }
    }

    fn index_of(&self, entry: &DirEntry) -> usize {
        self.entries
            .iter()
            .position(|candidate| candidate.name == entry.name)
            .unwrap_or(0)
    }

    // ── Rubber-band selection ──────────────────────────────────────────────

    /// The rubber-band rectangle, normalised so the corner order does not matter.
    fn drag_rect(&self) -> Option<(Point<Pixels>, Point<Pixels>)> {
        match (self.drag_anchor, self.drag_current) {
            (Some(a), Some(b)) => Some((
                gpui::point(a.x.min(b.x), a.y.min(b.y)),
                gpui::point(a.x.max(b.x), a.y.max(b.y)),
            )),
            _ => None,
        }
    }

    fn on_drag_start(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_anchor = Some(event.position);
        self.drag_current = Some(event.position);
        // Ctrl or Shift turns a fresh drag into an extension of the selection.
        self.drag_extend = event.modifiers.control || event.modifiers.shift;
        // Only a press on empty space clears the selection. A press that lands
        // on a tile may become that tile's drag, and clearing here would
        // destroy the very selection the drag is meant to move.
        let on_item = self
            .visible
            .iter()
            .any(|ix| self.row_contains(*ix, event.position));
        if !self.drag_extend && !on_item {
            self.selection.clear();
        }
        cx.notify();
    }

    fn on_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_anchor.is_none() {
            return;
        }
        self.drag_current = Some(event.position);
        let base = if self.drag_extend {
            self.selection.clone()
        } else {
            BTreeSet::new()
        };
        self.selection = base;
        for index in self.visible.clone() {
            if self.row_contains(index, event.position) {
                self.selection.insert(index);
            }
        }
        cx.notify();
    }

    fn on_drag_end(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.drag_anchor = None;
        self.drag_current = None;
        cx.notify();
    }

    /// Whether a window-relative point falls inside a row's tile.
    fn row_contains(&self, index: usize, at: Point<Pixels>) -> bool {
        let Some(slot) = self.visible.iter().position(|ix| *ix == index) else {
            return false;
        };
        match self.view_mode {
            ViewMode::Grid => grid_hit_test(self.grid_geometry, at, self.visible.len())
                .is_some_and(|(start, end)| (start..end).contains(&slot)),
            ViewMode::List => list_hit_test(self.list_geometry, at).is_some_and(|row| row == slot),
        }
    }
}

impl EventEmitter<BrowserEvent> for Browser {}

impl Focusable for Browser {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

// ── Hit testing ──────────────────────────────────────────────────────────────

/// Clamp a zoom change to the supported range.
///
/// The bounds live in [`crate::prefs`] so the persisted value and the live
/// clamp can never disagree.
pub fn step_zoom(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(prefs::ZOOM_RANGE.0, prefs::ZOOM_RANGE.1)
}

/// Number of grid tiles that fit across `container_w`.
pub fn grid_columns(container_w: f32, tile_w: f32, gap: f32) -> usize {
    if tile_w <= 0.0 || container_w <= 0.0 {
        return 1;
    }
    // `n` columns take up `n * tile_w` plus `n - 1` gaps, because the last
    // column has no gap after it. Solving `n * (tile_w + gap) <= container_w + gap`
    // for `n` gives the floor of that ratio, and `container_w + gap` is what
    // already accounts for the missing trailing gap.
    //
    // Rounding that floor *up* promised a column that did not fit, which is how
    // the rightmost column ended up clipped by the window edge.
    (((container_w + gap) / (tile_w + gap)).floor() as usize).max(1)
}

/// The half-open range of grid slots touched by `at`.
///
/// Returns `None` when the point is in the padding or in a gap between tiles, so
/// a click there clears the selection instead of picking a neighbouring row.
pub fn grid_hit_test(
    geometry: GridGeometry,
    at: Point<Pixels>,
    item_count: usize,
) -> Option<(usize, usize)> {
    let stride: f32 = (geometry.tile.width + px(GRID_GAP)).into();
    if stride <= 0.0 || geometry.columns == 0 {
        return None;
    }
    let dx: f32 = (at.x - geometry.origin.x).into();
    let dy: f32 = (at.y - geometry.origin.y).into();
    let col = (dx / stride).floor();
    let row = (dy / stride).floor();
    if col < 0.0 || row < 0.0 {
        return None;
    }
    let (col, row) = (col as usize, row as usize);
    if col >= geometry.columns {
        return None;
    }
    let start = row * geometry.columns + col;
    if start >= item_count {
        return None;
    }
    let tile_w: f32 = geometry.tile.width.into();
    let tile_h: f32 = geometry.tile.height.into();
    if dx % stride > tile_w || dy % stride > tile_h {
        return None;
    }
    Some((start, (start + 1).min(item_count)))
}

/// The list row under `at`, if any.
pub fn list_hit_test(geometry: ListGeometry, at: Point<Pixels>) -> Option<usize> {
    if geometry.row_h <= 0.0 {
        return None;
    }
    let offset: f32 = (at.y - geometry.origin_y).into();
    if offset < 0.0 {
        return None;
    }
    Some((offset / geometry.row_h).floor() as usize)
}

/// A short, readable name for a device serial.
///
/// Vendors prefix serials (`R5CT30ABCDE`), and a raw serial in a breadcrumb is
/// noise, so the distinctive tail is kept.
fn short_serial(serial: &str) -> String {
    if serial.chars().count() <= 8 {
        return serial.to_string();
    }
    serial
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// The context strip above the file area: what is being browsed, and how much of
/// it is currently visible.
fn context_bar(browser: &Browser, t: &theme::Palette) -> Div {
    let icon = if browser.is_local() {
        names::DRIVE_HARDDISK
    } else {
        names::PHONE
    };
    let title = if browser.is_local() {
        "This computer".to_string()
    } else {
        browser.device_label()
    };
    let shown = browser.visible.len();
    let total = browser.entries.len();
    let count = if shown == total {
        format!("{total} item(s)")
    } else {
        format!("{shown} of {total}")
    };

    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .mx(px(10.0))
        .mt(px(8.0))
        .px(px(14.0))
        .py(px(8.0))
        .rounded(px(theme::RADIUS_BAR))
        // A raised strip: T3 Code's canvas is near-black, so the context bar sits
        // on the `--surface-raised` step above it rather than below it.
        .bg(t.surface_raised)
        .border_1()
        .border_color(t.border_soft)
        .child(icons::icon(icon, 16.0, ui::icon_tint(icon, t)))
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text_header)
                        .child(title),
                )
                .child(ui::mono(browser.path().display().to_string(), t).mt(px(2.0))),
        )
        .child(div().flex_1())
        .child(ui::mono(count, t))
}

/// Shown when nothing is selected: what to do, in order.
fn empty_state(t: &theme::Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .size_full()
        .child(icons::icon(
            names::PHONE,
            40.0,
            ui::icon_tint(names::PHONE, t),
        ))
        .child(
            div()
                .text_size(px(19.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(t.text_header)
                .child("Connect your phone"),
        )
        .child(
            div()
                .max_w(px(360.0))
                .text_size(px(11.5))
                .line_height(relative(1.4))
                .text_color(t.text_secondary)
                .whitespace_normal()
                .child(
                    "Plug it in over USB and accept the debugging prompt, or pick a device \
                     in the sidebar to start browsing.",
                ),
        )
}

/// The bottom strip: item counts and transfer state.
/// `12.4 MB of 40.0 MB — 3.1 MB/s`, or just `Transferring` when the queue has
/// no byte counts to add up yet.
fn throughput(progress: &TransferProgress) -> String {
    if progress.bytes_total == 0 {
        return "Transferring".to_string();
    }
    let mut line = format!(
        "{} of {}",
        human_size(progress.bytes_done),
        human_size(progress.bytes_total)
    );
    if progress.speed_bps > 0 {
        line.push_str(&format!(" — {}/s", human_size(progress.speed_bps)));
    }
    line
}

/// The aggregate progress bar for the active jobs.
fn transfer_bar(browser: &Browser, t: &theme::Palette) -> AnyElement {
    let fraction = browser.progress.fraction();
    div()
        .w(px(120.0))
        .h(px(4.0))
        .rounded(px(2.0))
        .bg(t.border_soft)
        .child(
            div()
                .w(px(120.0 * fraction))
                .h_full()
                .rounded(px(2.0))
                .bg(t.accent),
        )
        .into_any_element()
}

fn status_bar(browser: &Browser, t: &theme::Palette, owner: &Entity<Browser>) -> AnyElement {
    let running = browser.active_jobs > 0;
    // A listing in flight outranks the item count: the count is the previous
    // directory's until the answer lands, so showing it would be a lie.
    let text = if browser.loading {
        "Loading…".to_string()
    } else if browser.status_text.is_empty() {
        format!("{} item(s)", browser.visible.len())
    } else {
        browser.status_text.clone()
    };

    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .h(px(theme::STATUSBAR_H))
        .px(px(14.0))
        .bg(t.statusbar)
        .border_t_1()
        .border_color(t.border_soft)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.5))
                .text_color(t.text_dim)
                .child(text),
        )
        .when(running, |d| {
            d.child(
                div()
                    .text_size(px(11.0))
                    .text_color(if browser.transfers_paused {
                        t.warning
                    } else {
                        t.text_dim
                    })
                    .child(if browser.transfers_paused {
                        "Transfers paused".to_string()
                    } else {
                        throughput(&browser.progress)
                    }),
            )
        })
        // The bar only means something while bytes are moving, so it is hidden
        // rather than shown empty next to a paused queue.
        .when(
            running && !browser.transfers_paused && browser.progress.bytes_total > 0,
            |d| d.child(transfer_bar(browser, t)),
        )
        // Pause and cancel act on every active job, which is what the GTK build
        // did from the same spot.
        .when(running, |d| {
            let pause = owner.clone();
            d.child(
                div()
                    .id("pause-transfers")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .child(icons::icon(
                        if browser.transfers_paused {
                            names::PLAY
                        } else {
                            names::PAUSE
                        },
                        12.0,
                        ui::icon_tint(
                            if browser.transfers_paused {
                                names::PLAY
                            } else {
                                names::PAUSE
                            },
                            t,
                        ),
                    ))
                    .on_mouse_down(MouseButton::Left, move |_, _w, cx| {
                        pause.update(cx, |_b, cx| {
                            cx.emit(BrowserEvent::TogglePauseTransfers);
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            )
        })
        .when(running, |d| {
            let cancel = owner.clone();
            d.child(
                div()
                    .id("cancel-transfers")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(22.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .child(icons::icon(
                        names::STOP,
                        12.0,
                        ui::icon_tint(names::STOP, t),
                    ))
                    .on_mouse_down(MouseButton::Left, move |_, _w, cx| {
                        cancel.update(cx, |_b, cx| {
                            cx.emit(BrowserEvent::CancelTransfers);
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            )
        })
        .into_any_element()
}

/// The stretched rectangle drawn while drag-selecting.
fn rubber_band(browser: &Browser, t: &theme::Palette) -> Option<AnyElement> {
    let (start, end) = browser.drag_rect()?;
    Some(
        div()
            .absolute()
            .left(start.x)
            .top(start.y)
            .w(end.x - start.x)
            .h(end.y - start.y)
            .border_1()
            .border_color(t.text_dim)
            .bg(t.secondary_bg)
            .into_any_element(),
    )
}

/// The full-colour folder for a directory, chosen by name where that reads
/// better than a plain folder.
///
/// These are drawn through [`icons::artwork`], not [`icons::icon`]: gpui's
/// `svg()` keeps only a silhouette's alpha channel and tints it, so the desktop
/// folder would come out a featureless blob. See the note on the asset table.
fn folder_art_for(entry: &DirEntry) -> &'static str {
    let lower = entry.name.to_lowercase();
    if lower == "documents" || lower == "document" {
        names::folders::DOCUMENTS
    } else if lower == "downloads" || lower == "download" {
        names::folders::DOWNLOAD
    } else if lower == "music" {
        names::folders::MUSIC
    } else if lower == "videos" || lower == "movies" || lower == "video" {
        names::folders::VIDEOS
    } else if lower.contains("screenshot")
        || lower.contains("camera")
        || lower == "dcim"
        || lower == "pictures"
    {
        names::folders::PICTURES
    } else {
        names::folders::FOLDER
    }
}

/// Full-colour artwork for the file types the design has bespoke art for, or
/// `None` to fall back to a monochrome glyph.
fn artwork_for(entry: &DirEntry) -> Option<&'static str> {
    use crate::icons::names::art;
    Some(match entry.ext().as_str() {
        "apk" => art::APK,
        "zip" | "7z" | "rar" => art::ZIP,
        "tar" | "gz" | "tgz" | "xz" => art::TAR,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odg"
        | "csv" | "epub" => art::DOCUMENTS,
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" => art::PODCASTS,
        "txt" | "log" | "json" | "xml" => art::TXT,
        "mp4" | "mkv" | "avi" | "webm" | "mov" => art::MOVIE,
        "png" | "jpg" | "jpeg" | "webp" | "gif" => art::WALLPAPER,
        _ => return None,
    })
}

impl Browser {
    /// Handle a click on a row or tile: select it, and on a double click, open it.
    fn on_item_click(
        &mut self,
        index: usize,
        event: &gpui::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let extend = if event.modifiers().shift {
            Extend::Range
        } else if event.modifiers().control {
            Extend::Toggle
        } else {
            Extend::None
        };
        self.click_row(index, extend);
        if event.click_count() >= 2
            && let Some(outcome) = self.open_index(index)
        {
            cx.emit(outcome.into_event());
        }
        cx.notify();
    }

    /// Right-clicking a row retargets the selection to it unless it is already
    /// part of it, so the menu always acts on what the user pointed at.
    fn on_item_right_click(
        &mut self,
        index: usize,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selection.contains(&index) {
            self.click_row(index, Extend::None);
        }
        self.focused = Some(index);
        cx.notify();
        cx.emit(BrowserEvent::ContextMenu {
            position: event.position,
            focused: Some(index),
        });
    }

    /// The monochrome glyph for a file. Folders never reach here: they are drawn
    /// full-colour by [`folder_art_for`].
    fn glyph_for(&self, entry: &DirEntry) -> &'static str {
        match entry.ext().as_str() {
            "txt" | "log" | "json" | "xml" => names::TEXT_GENERIC,
            "png" | "jpg" | "jpeg" | "webp" | "gif" => names::IMAGE_GENERIC,
            "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" => names::AUDIO_GENERIC,
            "mp4" | "mkv" | "avi" | "webm" | "mov" => names::VIDEO_GENERIC,
            "apk" => names::PACKAGE,
            _ => names::DOCUMENT,
        }
    }

    /// One tile of the grid view.
    /// Make an item a drag source, and — when it is a directory — a drop target
    /// for both kinds of payload.
    ///
    /// A file cannot accept a drop, so only directories get `on_drop`; without
    /// that guard GPUI would happily route a drop over a file row to its
    /// container and the move would land somewhere the user never pointed at.
    fn draggable_item(
        &self,
        tile: impl StatefulInteractiveElement + IntoElement,
        entry: &DirEntry,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ghost = self.drag_ghost.clone();
        // Dragging a tile that is part of the selection moves the selection, not
        // just that one tile.
        let payload = DraggedEntries(if self.selection.contains(&index) {
            self.entries
                .iter()
                .enumerate()
                .filter(|(i, _)| self.selection.contains(i))
                .map(|(_, e)| e.clone())
                .collect()
        } else {
            vec![entry.clone()]
        });
        let tile = tile.on_drag(payload, move |payload, _offset, _window, cx| {
            let label = DragGhost::describe(payload);
            ghost.update(cx, |ghost, cx| {
                ghost.label = label;
                cx.notify();
            });
            ghost.clone()
        });

        if !entry.looks_like_dir() {
            return tile.into_any_element();
        }

        let folder = entry.clone();
        let external_folder = folder.clone();
        let internal_folder = folder.clone();
        tile.on_drop::<ExternalPaths>(cx.listener(
            move |this, dropped: &ExternalPaths, _window, cx| {
                let paths = dropped.paths().to_vec();
                cx.emit(BrowserEvent::DroppedOnFolder(
                    external_folder.clone(),
                    paths,
                ));
                this.hovered_drop_folder = None;
            },
        ))
        .on_drop::<DraggedEntries>(cx.listener(
            move |this, dragged: &DraggedEntries, _window, cx| {
                cx.emit(BrowserEvent::DraggedOntoFolder(
                    internal_folder.clone(),
                    dragged.0.clone(),
                ));
                this.hovered_drop_folder = None;
            },
        ))
        .on_drag_move::<ExternalPaths>(cx.listener(
            move |this, _event: &gpui::DragMoveEvent<ExternalPaths>, _w, cx| {
                this.hovered_drop_folder = Some(folder.name.clone());
                cx.notify();
            },
        ))
        .into_any_element()
    }

    fn grid_tile(&self, slot: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        // The palette is `Copy`, so take a value: a borrow of `cx` would collide
        // with the closures below that need `&mut cx`.
        let t = *cx.theme();
        let index = *self.visible.get(slot)?;
        let entry = self.entries.get(index)?.clone();
        let selected = self.selection.contains(&index);
        let focused = self.focused == Some(index);
        let icon_size = self.zoom;
        let tile_w = icon_size + GRID_TILE_PAD * 2.0;
        let tile_h = tile_w + GRID_TILE_CAPTION;

        let icon_el: AnyElement = match artwork_for(&entry) {
            Some(art) if !entry.looks_like_dir() => {
                icons::artwork(art, icon_size).into_any_element()
            }
            _ if entry.looks_like_dir() => {
                icons::artwork(folder_art_for(&entry), icon_size).into_any_element()
            }
            _ => icons::icon(
                self.glyph_for(&entry),
                icon_size * 0.9,
                if selected {
                    t.text_header
                } else {
                    ui::icon_tint(self.glyph_for(&entry), &t)
                },
            )
            .into_any_element(),
        };

        // One div per piece of text rather than a wrapper plus a label: at
        // ~0.023ms per element, the two wrappers this tile used to spend on the
        // name and the size were about a third of its layout cost for nothing.
        let label = div()
            // So a test can measure the laid-out box. Note what this does *not*
            // prove: `debug_bounds` reports the element's box, not the width the
            // text was laid out in, so it reads a correct width even while every
            // name renders as an ellipsis. It guards the tile's geometry, not the
            // text.
            .debug_selector(move || format!("tile-label-{index}"))
            .mt(px(6.0))
            // An explicit width, not `w_full()`. The tile centres its children,
            // so a percentage width here resolves against nothing and the box
            // falls back to sizing itself from its content — with `min_w_0` that
            // means collapsing to the shortest thing that can be drawn, which is
            // an ellipsis. Every name read as "...".
            //
            // The tile's own 8px of horizontal padding is taken out here, so the
            // label is exactly as wide as the tile's content box and `truncate()`
            // has a definite box to ellipsize inside. The size line below already
            // worked this way, which is why the sizes were readable while the
            // names were not.
            .w(px(tile_w - 8.0))
            .px(px(2.0))
            .min_w_0()
            // The box is now the full width of the tile, so the text inside it
            // needs centring or names sit against the tile's left edge.
            .text_center()
            .text_size(px(11.5))
            .font_weight(if entry.looks_like_dir() {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            })
            .text_color(if selected {
                t.text_header
            } else {
                t.text_secondary
            })
            .truncate()
            .child(entry.name.clone());

        // A folder has no meaningful size, and an em dash under every folder was
        // just noise. Only files get the second line.
        let sub = (!entry.is_dir).then(|| {
            div()
                .mt(px(1.0))
                .w(px(tile_w - 8.0))
                .min_w_0()
                .text_center()
                .font_family(theme::MONO)
                .text_size(px(9.5))
                .text_color(t.text_muted)
                .truncate()
                .child(human_size(entry.size))
        });

        // The tile's own background, resolved once rather than by three
        // overlapping style callbacks.
        let resting_bg = if selected {
            Some(t.selected)
        } else if focused {
            Some(t.hover)
        } else {
            None
        };
        let hover_bg = if selected { t.selected_strong } else { t.hover };

        let tile = div()
            .id(("tile", index))
            .flex()
            .flex_col()
            .items_center()
            .w(px(tile_w))
            .h(px(tile_h))
            .pt(px(GRID_TILE_PAD))
            .px(px(4.0))
            .pb(px(6.0))
            .rounded(px(10.0))
            .cursor_pointer()
            // A tile is exactly `tile_w` wide by construction, and
            // `grid_geometry` hit-tests against that width. Left shrinkable, a
            // row that overflowed its pane squeezed the tiles instead of
            // clipping, and every measurement downstream was then wrong:
            // `grid_columns` sizes rows for `tile_w`, hit testing assumed
            // `tile_w`, and the name label — which is a percentage of the tile —
            // shrank with it until long names were nothing but an ellipsis.
            //
            // Refusing to shrink turns any residual overflow into clipping,
            // which is visible, rather than silent corruption.
            .flex_shrink_0()
            .when_some(resting_bg, |d, bg| d.bg(bg))
            .hover(move |d| d.bg(hover_bg))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(icon_size))
                    .w_full()
                    .child(icon_el),
            )
            .child(label)
            .when_some(sub, |d, sub| d.child(sub))
            .on_click(
                cx.listener(move |this, event, w, cx| this.on_item_click(index, event, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event, w, cx| {
                    this.on_item_right_click(index, event, w, cx)
                }),
            );

        Some(
            self.draggable_item(tile, &entry, index, cx)
                .into_any_element(),
        )
    }

    /// One row of the list view, with the palette passed in.
    ///
    /// `uniform_list`'s processor runs *during* this entity's own render, so it
    /// cannot read the theme through the context: doing so would re-enter an
    /// update that is already in flight. The palette is captured once per frame
    /// instead.
    fn list_row_with(
        &self,
        slot: usize,
        t: &theme::Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let index = *self.visible.get(slot)?;
        let entry = self.entries.get(index)?.clone();
        let selected = self.selection.contains(&index);
        let focused = self.focused == Some(index);
        let size = if entry.is_dir {
            "—".to_string()
        } else {
            human_size(entry.size)
        };

        let row = div()
            .id(("row", index))
            .flex()
            .items_center()
            .gap(px(10.0))
            .w_full()
            .h(px(LIST_ROW_H))
            .mx(px(10.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .when(selected, |d| d.bg(t.hover))
            .when(focused && !selected, |d| d.bg(t.hover))
            .hover(|d| d.bg(if selected { t.pressed } else { t.hover }))
            .child(if entry.looks_like_dir() {
                icons::artwork(folder_art_for(&entry), 18.0).into_any_element()
            } else {
                icons::icon(
                    self.glyph_for(&entry),
                    15.0,
                    if selected {
                        t.text_header
                    } else {
                        ui::icon_tint(self.glyph_for(&entry), t)
                    },
                )
                .into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if selected {
                        t.text_header
                    } else {
                        t.text_primary
                    })
                    .child(entry.name.clone()),
            )
            .child(ui::mono(size, t).w(px(72.0)).text_right())
            .child(ui::mono(entry.mtime_string(), t).w(px(118.0)).text_right())
            .on_click(
                cx.listener(move |this, event, w, cx| this.on_item_click(index, event, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event, w, cx| {
                    this.on_item_right_click(index, event, w, cx)
                }),
            );

        Some(
            self.draggable_item(row, &entry, index, cx)
                .into_any_element(),
        )
    }
}

impl Render for Browser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The palette is `Copy`, so take a value: a borrow of `cx` would collide
        // with the closures below that need `&mut cx`.
        let t = *cx.theme();
        let me = cx.entity();

        if !self.has_device() {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .bg(t.canvas)
                .child(empty_state(&t))
                .into_any_element();
        }

        // Work out the item geometry now, so a drag can hit-test without asking
        // the compositor for bounds. The app root reports where the browser's own
        // box sits; the browser owns the chrome it stacks above the items, so
        // that offset is added here rather than guessed at across the two.
        let tile_w = self.zoom + GRID_TILE_PAD * 2.0;
        let item_top = self.viewport_y + self.item_area_top();
        self.grid_geometry = GridGeometry {
            origin: gpui::point(px(self.viewport_x + GRID_GAP), px(item_top)),
            tile: Size {
                width: px(tile_w),
                height: px(tile_w + GRID_TILE_CAPTION),
            },
            columns: grid_columns(self.viewport_width, tile_w, GRID_GAP),
        };
        self.list_geometry = ListGeometry {
            origin_y: px(item_top),
            row_h: LIST_ROW_H,
        };

        let count = self.visible.len();
        let empty_message = if self.entries.is_empty() {
            "This folder is empty."
        } else {
            "Nothing matches your search."
        };

        let body: AnyElement = if count == 0 {
            div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_size(px(12.0))
                .text_color(t.text_muted)
                .child(empty_message)
                .into_any_element()
        } else {
            match self.view_mode {
                ViewMode::Grid => {
                    let tile_w = self.tile_width();
                    let tile_h = tile_w + GRID_TILE_CAPTION + GRID_GAP;
                    let columns = self.grid_geometry.columns.max(1);
                    let rows = count.div_ceil(columns);

                    // Only build the rows that can be on screen. A phone's DCIM
                    // runs to thousands of files, and building a tile for each
                    // one every frame cost ~160ms at 5,000 entries; windowing
                    // keeps this proportional to the viewport instead.
                    let (first_row, visible_rows) = self.visible_row_range(tile_h);
                    let total_height = rows as f32 * tile_h + GRID_TILE_PAD;

                    #[cfg(test)]
                    {
                        self.last_built_tiles =
                            (visible_rows * self.grid_geometry.columns.max(1)).min(count);
                    }
                    let windowed: Vec<AnyElement> = (first_row..first_row + visible_rows)
                        .map(|row| {
                            let first = row * columns;
                            let last = (first + columns).min(count);
                            div()
                                .flex()
                                .gap(px(GRID_GAP))
                                .px(px(GRID_GAP))
                                .children((first..last).filter_map(|slot| self.grid_tile(slot, cx)))
                                .into_any_element()
                        })
                        .collect();

                    div()
                        .id("grid")
                        .relative()
                        .w_full()
                        .h(px(total_height))
                        .child(
                            div()
                                .absolute()
                                .top(px(GRID_TILE_PAD + first_row as f32 * tile_h))
                                .left_0()
                                .right_0()
                                .flex()
                                .flex_col()
                                .gap(px(GRID_GAP))
                                .children(windowed),
                        )
                        .into_any_element()
                }
                ViewMode::List => {
                    // The processor already hands over `&mut Browser`. Reaching
                    // for the entity from inside it would re-enter an update
                    // that is already in progress, which GPUI treats as a
                    // double lease and panics on.
                    let palette = t;
                    uniform_list(
                        "list",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                            range
                                .filter_map(|slot| this.list_row_with(slot, &palette, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .h_full()
                    .into_any_element()
                }
            }
        };

        let scroller = div()
            .id("canvas")
            .flex_1()
            .min_h_0()
            .relative()
            .overflow_y_scroll()
            .scrollbar_width(px(8.0))
            .track_scroll(&self.scroll)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_drag_start))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_drag_end))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_drag_end))
            // Dropping on empty canvas means "into the directory being browsed".
            // Rows sit on top of this and take the drop first, so a drop over a
            // row is never mistaken for a drop over the directory.
            .on_drop::<ExternalPaths>(cx.listener(|this, dropped: &ExternalPaths, _window, cx| {
                let paths = dropped.paths().to_vec();
                cx.emit(BrowserEvent::DroppedPaths(paths));
                this.hovered_drop_folder = None;
            }))
            .child(body)
            .when_some(rubber_band(self, &t), |d, band| d.child(band));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.canvas)
            .track_focus(&self.focus)
            .key_context(BROWSER_CONTEXT)
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::deselect))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::upload))
            .on_action(cx.listener(Self::download))
            .on_action(cx.listener(Self::toggle_hidden))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::toggle_view))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::rename_focused))
            .on_action(cx.listener(Self::delete_selection))
            .on_action(cx.listener(Self::delete_permanently))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::forward))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::open_focused))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::toggle_path_entry))
            .on_action(cx.listener(Self::focus_previous))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_first))
            .on_action(cx.listener(Self::focus_last))
            .child(context_bar(self, &t))
            .child(scroller)
            .child(status_bar(self, &t, &me))
            .into_any_element()
    }
}

/// A synthetic Android media listing for the benchmarks.
///
/// Outside the test module so the app-level benchmark builds the same fixture the
/// browser benchmarks use, which keeps the two comparable.
#[cfg(test)]
pub fn synthetic_listing(count: usize) -> Vec<DirEntry> {
    const STEMS: &[&str] = &[
        "IMG_20240101_120000",
        "Screenshot_2024-01-03",
        "video_20240104",
        "document",
        "archive",
        "notes",
        "podcast_episode",
    ];
    const EXTS: &[&str] = &["jpg", "png", "mp4", "pdf", "zip", "txt", "mp3"];

    (0..count)
        .map(|i| DirEntry {
            name: format!("{}_{i:04}.{}", STEMS[i % STEMS.len()], EXTS[i % EXTS.len()]),
            is_dir: i % 25 == 0,
            is_symlink: false,
            size: 1_000_000 + (i as u64 * 7919),
            mode: 0o644,
            mtime: 1_700_000_000 + i as i64,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

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

    /// Install the palette and open a window hosting a browser.
    ///
    /// A `Browser` owns a `FocusHandle`, which can only be built from an `App`,
    /// so the tests that need an instance go through the real harness.
    fn open(cx: &mut TestAppContext) -> gpui::WindowHandle<Browser> {
        cx.update(crate::theme::install);
        cx.add_window(|_, cx| Browser::new(cx))
    }

    // ── Pure geometry: no instance needed ──────────────────────────────────

    #[test]
    fn short_serial_keeps_the_distinctive_tail() {
        // "emulator-5554" is 13 characters, so the last 8 are kept.
        assert_eq!(short_serial("emulator-5554"), "tor-5554");
        assert_eq!(short_serial("R5CT30ABCDE"), "T30ABCDE");
        assert_eq!(
            short_serial("12345678"),
            "12345678",
            "a serial that is already eight characters passes through"
        );
    }

    #[gpui::test]
    async fn a_dropped_listing_does_not_wedge_the_browser(cx: &mut TestAppContext) {
        // Regression: the caller dropped a listing whose device/path no longer
        // matched, returned without clearing `loading`, and `begin_load` then
        // refused every request forever — no directory could be listed again,
        // including "This computer".
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            let accepted = browser.update(app, |b, cx| {
                b.set_device("A", "Phone A", cx);
                b.navigate(PathBuf::from("/sdcard"));
                assert!(b.begin_load(cx), "the first load starts");
                // The user switched phones while the listing was in flight, so
                // the answer comes back stale.
                b.set_device("B", "Phone B", cx);
                b.cancel_load(cx);
                b.begin_load(cx)
            });
            assert!(
                accepted,
                "the browser still accepts requests after a discarded listing"
            );
        });
    }

    #[gpui::test]
    async fn filtering_drops_a_large_selection_without_going_quadratic(cx: &mut TestAppContext) {
        // Regression: pruning the selection scanned `visible` linearly for every
        // selected row, so select-all followed by a keystroke was O(n^2). At
        // 5,000 entries that is 25 million comparisons, which is a multi-second
        // freeze on a slow machine.
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| {
                b.device = Some("bench".into());
                b.install_entries(synthetic_listing(5_000));
                b.select_all_entries();
                let _ = cx;
                assert_eq!(b.selected().len(), 5_000, "precondition: all selected");

                let start = std::time::Instant::now();
                b.set_search_query_raw("zzz-no-such-file".into());
                let elapsed = start.elapsed();

                assert!(
                    b.selected().is_empty(),
                    "everything is filtered out, so nothing may stay selected"
                );
                assert!(
                    elapsed.as_millis() < 200,
                    "pruning took {elapsed:?}, which suggests the quadratic path is back"
                );
            });
        });
    }

    #[gpui::test]
    async fn the_item_area_starts_below_the_browsers_own_chrome(cx: &mut TestAppContext) {
        // Regression: the rubber-band hit-test origin used to be the browser's
        // own top edge, which sits above the context strip, so a drag selected
        // rows ~50px higher than the pointer was over. The offset belongs to the
        // browser because the context strip and search row are its children.
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| {
                b.device = Some("s".into());
                b.set_viewport(232.0, 45.0, 768.0, 600.0, cx);

                // Context strip plus the grid's own padding, and no search row.
                assert_eq!(b.item_area_top(), CONTEXT_BAR_H + GRID_TILE_PAD);

                // Turning search on pushes the items down by the strip's height.
                b.search_active = true;
                assert_eq!(
                    b.item_area_top(),
                    CONTEXT_BAR_H + SEARCH_ROW_H + GRID_TILE_PAD,
                    "the search row has to be accounted for too"
                );

                // The item area is what is left of the box.
                let top = b.item_area_top();
                assert_eq!(b.item_area_height(), 600.0 - top);
            });
        });
    }

    #[gpui::test]
    async fn the_grid_window_covers_the_viewport_and_no_more(cx: &mut TestAppContext) {
        // Regression guard for the windowing that replaced building a tile for
        // every entry: a folder of thousands of files must cost the same as a
        // small one, and the window must still cover the whole viewport.
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| {
                b.device = Some("bench".into());
                b.install_entries(synthetic_listing(5_000));
                // 8 columns of 76px tiles in a 768x560 file area.
                b.set_viewport(232.0, 45.0, 768.0, 560.0, cx);
            });
        });

        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            let b = browser.read(app);
            let tile_h = b.tile_width() + GRID_TILE_CAPTION + GRID_GAP;
            let (first, rows) = b.visible_row_range(tile_h);
            let expected_rows = (560.0 / tile_h).ceil() as usize + 3;
            assert_eq!(first, 0, "at rest the window starts at the first row");
            assert!(
                rows <= expected_rows + 1,
                "built {rows} rows for a viewport needing about {expected_rows}"
            );
            assert!(rows >= 6, "the window must cover a 560px viewport");
        });
    }

    #[gpui::test]
    async fn the_grid_window_follows_the_scroll_offset(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| {
                b.device = Some("bench".into());
                b.install_entries(synthetic_listing(5_000));
                b.set_viewport(232.0, 45.0, 768.0, 560.0, cx);
            });
        });
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, _cx| b.scroll_to(4_000.0));
        });
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            let b = browser.read(app);
            let tile_h = b.tile_width() + GRID_TILE_CAPTION + GRID_GAP;
            let (first, _) = b.visible_row_range(tile_h);
            assert!(
                first > 20,
                "scrolling 4000px must move the window down, got row {first}"
            );
        });
    }

    #[gpui::test]
    async fn the_grid_window_falls_back_when_there_is_no_layout_yet(cx: &mut TestAppContext) {
        // Before the first frame there is no viewport, so the window must stay
        // small rather than building the whole folder.
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, _cx| {
                b.device = Some("bench".into());
                b.install_entries(synthetic_listing(5_000));
            });
            let browser = handle.root(app).expect("root view");
            let b = browser.read(app);
            let (first, rows) = b.visible_row_range(130.0);
            assert_eq!(first, 0);
            assert!(rows <= 32, "fallback window was {rows} rows");
        });
    }

    #[gpui::test]
    async fn the_grid_content_height_covers_every_row(cx: &mut TestAppContext) {
        // The window only builds some rows, so something has to keep the
        // scrollable height honest or the scrollbar would collapse.
        let handle = open(cx);
        for count in [1usize, 8, 100, 5_000] {
            cx.update(|app| {
                let browser = handle.root(app).expect("root view");
                browser.update(app, |b, cx| {
                    b.install_entries(synthetic_listing(count));
                    b.set_viewport(232.0, 45.0, 768.0, 560.0, cx);
                });
            });
            cx.update(|app| {
                let browser = handle.root(app).expect("root view");
                let b = browser.read(app);
                let columns = b.grid_geometry.columns.max(1);
                let rows = b.visible.len().div_ceil(columns);
                let tile_w = b.tile_width();
                let height = rows as f32 * (tile_w + GRID_TILE_CAPTION + GRID_GAP) + GRID_TILE_PAD;
                assert!(
                    height > 0.0,
                    "{count} items must have a positive content height"
                );
                if count > 100 {
                    assert!(
                        height > b.viewport_height,
                        "{count} items must overflow the viewport, got {height}"
                    );
                }
            });
        }
    }

    #[test]
    fn zoom_is_clamped_to_its_range() {
        let mut zoom = prefs::ZOOM_DEFAULT;
        for _ in 0..40 {
            zoom = step_zoom(zoom, ZOOM_STEP);
        }
        assert_eq!(zoom, prefs::ZOOM_RANGE.1, "zoom in stops at the maximum");
        for _ in 0..60 {
            zoom = step_zoom(zoom, -ZOOM_STEP);
        }
        assert_eq!(zoom, prefs::ZOOM_RANGE.0, "zoom out stops at the minimum");
        assert_eq!(
            step_zoom(zoom, ZOOM_STEP),
            prefs::ZOOM_RANGE.0 + ZOOM_STEP,
            "and it can come back"
        );
    }

    #[test]
    fn grid_columns_accounts_for_gaps_and_degenerate_input() {
        // Four 100px tiles and three 8px gaps is 424px, which fits in 500; a
        // fifth would need 532 and would be clipped by the edge.
        assert_eq!(grid_columns(500.0, 100.0, GRID_GAP), 4);
        assert_eq!(
            grid_columns(0.0, 100.0, GRID_GAP),
            1,
            "a zero-width container"
        );
        assert_eq!(grid_columns(500.0, 0.0, GRID_GAP), 1, "a zero-width tile");
        assert_eq!(
            grid_columns(-10.0, 100.0, GRID_GAP),
            1,
            "a negative container"
        );
    }

    /// Whatever `grid_columns` promises has to actually fit.
    ///
    /// It used to round up, so the last column was laid out past the right edge
    /// and clipped — a whole column of files the user could not click.
    #[test]
    fn the_last_grid_column_is_not_clipped() {
        for container in [320.0_f32, 500.0, 768.0, 1000.0, 1672.0] {
            for tile in [40.0_f32, 76.0, 100.0, 148.0] {
                let columns = grid_columns(container, tile, GRID_GAP);
                let used = columns as f32 * tile + (columns - 1) as f32 * GRID_GAP;
                assert!(
                    used <= container,
                    "{columns} tiles of {tile}px need {used}px in {container}px"
                );
                // ...and it should not be shy either: one more column has to
                // overflow, or the grid is wasting the space it was given.
                let one_more = (columns + 1) as f32 * tile + columns as f32 * GRID_GAP;
                assert!(
                    one_more > container,
                    "{container}px holds more than {columns} tiles of {tile}px"
                );
            }
        }
    }

    #[test]
    fn grid_hit_test_maps_points_to_slots() {
        let geometry = GridGeometry {
            origin: gpui::point(px(0.), px(0.)),
            tile: Size {
                width: px(100.),
                height: px(80.),
            },
            columns: 3,
        };

        // The stride is the tile plus its gap: 108 wide, 108 tall.
        assert_eq!(
            grid_hit_test(geometry, gpui::point(px(10.), px(10.)), 9),
            Some((0, 1)),
            "the first tile"
        );
        assert_eq!(
            grid_hit_test(geometry, gpui::point(px(120.), px(120.)), 9),
            Some((4, 5)),
            "column 1, row 1 is slot 1*3+1 = 4"
        );
        assert_eq!(
            grid_hit_test(geometry, gpui::point(px(220.), px(220.)), 9),
            Some((8, 9)),
            "the last tile clamps to the item count"
        );
    }

    #[test]
    fn grid_hit_test_ignores_gaps_and_out_of_range_points() {
        let geometry = GridGeometry {
            origin: gpui::point(px(0.), px(0.)),
            tile: Size {
                width: px(100.),
                height: px(80.),
            },
            columns: 3,
        };

        for (point, why) in [
            (gpui::point(px(105.), px(10.)), "the gap between columns"),
            (gpui::point(px(10.), px(500.)), "below every row"),
            (gpui::point(px(-5.), px(10.)), "left of the origin"),
            (gpui::point(px(320.), px(10.)), "past the last column"),
        ] {
            assert_eq!(grid_hit_test(geometry, point, 9), None, "{why}");
        }
        assert_eq!(
            grid_hit_test(geometry, gpui::point(px(220.), px(180.)), 4),
            None,
            "a slot past the item count is not a hit"
        );
        assert_eq!(
            grid_hit_test(GridGeometry::default(), gpui::point(px(1.), px(1.)), 9),
            None,
            "an unset geometry never hits"
        );
    }

    #[test]
    fn list_hit_test_maps_points_to_rows() {
        let geometry = ListGeometry {
            origin_y: px(10.),
            row_h: 30.0,
        };
        assert_eq!(
            list_hit_test(geometry, gpui::point(px(5.), px(10.))),
            Some(0)
        );
        assert_eq!(
            list_hit_test(geometry, gpui::point(px(5.), px(39.))),
            Some(0)
        );
        assert_eq!(
            list_hit_test(geometry, gpui::point(px(5.), px(41.))),
            Some(1)
        );
        assert_eq!(
            list_hit_test(geometry, gpui::point(px(5.), px(5.))),
            None,
            "above the first row"
        );
        assert_eq!(
            list_hit_test(
                ListGeometry {
                    origin_y: px(0.),
                    row_h: 0.0
                },
                gpui::point(px(1.), px(1.))
            ),
            None,
            "a zero row height never hits"
        );
    }

    #[test]
    fn open_outcomes_map_to_the_events_the_app_handles() {
        let e = entry("x", false);
        assert_eq!(
            OpenOutcome::Directory(PathBuf::from("/a")).into_event(),
            BrowserEvent::OpenedDirectory(PathBuf::from("/a"))
        );
        assert_eq!(
            OpenOutcome::InstallApk(e.clone()).into_event(),
            BrowserEvent::InstallApk(e.clone())
        );
        assert_eq!(
            OpenOutcome::Download(e.clone()).into_event(),
            BrowserEvent::Open(e)
        );
    }

    // ── Icon mapping ───────────────────────────────────────────────────────

    #[test]
    fn folders_get_a_variant_by_name() {
        use crate::icons::names::folders;
        for (name, expected) in [
            ("Documents", folders::DOCUMENTS),
            ("Downloads", folders::DOWNLOAD),
            ("Music", folders::MUSIC),
            ("Videos", folders::VIDEOS),
            ("DCIM", folders::PICTURES),
            ("Screenshots", folders::PICTURES),
            ("random", folders::FOLDER),
        ] {
            assert_eq!(folder_art_for(&entry(name, true)), expected, "{name}");
        }
    }

    #[test]
    fn every_folder_asset_is_actually_embedded() {
        // A typo in a name here would render nothing at all, and `img()` fails
        // silently — so assert each key resolves to real bytes.
        use crate::icons::names::folders;
        for key in [
            folders::FOLDER,
            folders::DOCUMENTS,
            folders::DOWNLOAD,
            folders::MUSIC,
            folders::OPEN,
            folders::PICTURES,
            folders::VIDEOS,
        ] {
            let found = crate::icons::ASSETS.iter().find(|(name, _)| *name == key);
            assert!(found.is_some(), "{key} is not in the asset table");
            assert!(
                found.unwrap().1.starts_with(b"\x89PNG"),
                "{key} is not a PNG, so artwork() would not decode it"
            );
        }
    }

    #[test]
    fn artwork_covers_the_types_the_design_has_art_for() {
        use crate::icons::names::art;
        for (name, expected) in [
            ("a.apk", art::APK),
            ("a.zip", art::ZIP),
            ("a.7z", art::ZIP),
            ("a.tar.gz", art::TAR),
            ("a.pdf", art::DOCUMENTS),
            ("a.epub", art::DOCUMENTS),
            ("a.mp3", art::PODCASTS),
            ("a.txt", art::TXT),
            ("a.json", art::TXT),
            ("a.mp4", art::MOVIE),
            ("a.png", art::WALLPAPER),
        ] {
            assert_eq!(artwork_for(&entry(name, false)), Some(expected), "{name}");
        }
        assert_eq!(artwork_for(&entry("noextension", false)), None);
        assert_eq!(artwork_for(&entry("a.bin", false)), None);
    }

    #[test]
    fn every_referenced_asset_is_embedded() {
        use crate::icons::names::art;
        // The icon constants already carry their full asset key, so this is just
        // the list of what the browser can ask for.
        let mut referenced: Vec<&str> = vec![
            folder_art_for(&entry("x", true)),
            names::PHONE,
            names::DRIVE_HARDDISK,
            names::TEXT_GENERIC,
            names::IMAGE_GENERIC,
            names::AUDIO_GENERIC,
            names::VIDEO_GENERIC,
            names::PACKAGE,
            names::DOCUMENT,
            art::APK,
            art::ZIP,
            art::TAR,
            art::DOCUMENTS,
            art::PODCASTS,
            art::TXT,
            art::MOVIE,
            art::WALLPAPER,
        ];
        referenced.sort_unstable();
        referenced.dedup();

        for path in referenced {
            let found = crate::icons::ASSETS
                .iter()
                .find(|(asset, _)| *asset == path)
                .unwrap_or_else(|| {
                    panic!(
                        "{path} is referenced by the browser but not \
                 embedded, so it would render as nothing"
                    )
                });
            // The artwork is what carries this browser's colour, so it has to be
            // a PNG: `img()` cannot decode an SVG and would draw nothing.
            if path.starts_with("art/") {
                assert!(
                    found.1.starts_with(b"\x89PNG"),
                    "{path} is file-type artwork, so it must be a PNG"
                );
            }
        }
    }

    // ── State: these need a real entity, hence the harness ─────────────────

    #[gpui::test]
    #[gpui::test]
    async fn shift_click_selects_the_range_between_the_anchor_and_the_click(
        cx: &mut TestAppContext,
    ) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![
                    entry("a", true),
                    entry("b", true),
                    entry("c", true),
                    entry("d", true),
                    entry("e", true),
                ])
            });

            b.update(app, |b, _| b.click_row(1, Extend::None));
            b.update(app, |b, _| b.click_row(3, Extend::Range));
            let names: Vec<String> = b
                .read(app)
                .selected()
                .iter()
                .map(|e| e.name.clone())
                .collect();
            assert_eq!(
                names,
                vec!["b", "c", "d"],
                "shift fills every row between the anchor and the click"
            );

            // Backwards, because dragging the band up is still a range.
            b.update(app, |b, _| b.click_row(4, Extend::None));
            b.update(app, |b, _| b.click_row(0, Extend::Range));
            let names: Vec<String> = b
                .read(app)
                .selected()
                .iter()
                .map(|e| e.name.clone())
                .collect();
            assert_eq!(names, vec!["a", "b", "c", "d", "e"]);
        });
    }

    #[gpui::test]
    async fn shift_click_without_an_anchor_selects_only_the_clicked_row(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", true), entry("b", true), entry("c", true)])
            });
            b.update(app, |b, _| b.click_row(2, Extend::Range));
            assert_eq!(
                b.read(app).selected().len(),
                1,
                "with no anchor there is no range to extend"
            );
        });
    }

    #[gpui::test]
    async fn clicking_replaces_the_selection_and_ctrl_toggles_it(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", true), entry("b", true), entry("c", false)])
            });

            b.update(app, |b, _| b.click_row(0, Extend::None));
            assert_eq!(b.read(app).selected().len(), 1);

            b.update(app, |b, _| b.click_row(2, Extend::None));
            assert_eq!(
                b.read(app).selected().len(),
                1,
                "a plain click replaces the selection"
            );
            assert_eq!(b.read(app).selected()[0].name, "c");

            b.update(app, |b, _| b.click_row(0, Extend::Toggle));
            assert_eq!(
                b.read(app).selected().len(),
                2,
                "ctrl adds to the selection"
            );

            b.update(app, |b, _| b.click_row(0, Extend::Toggle));
            assert_eq!(
                b.read(app).selected().len(),
                1,
                "ctrl on an already-selected row removes it"
            );
        });
    }

    #[gpui::test]
    async fn focus_moves_wrap_around(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", true), entry("b", true), entry("c", true)])
            });

            b.update(app, |b, _| b.focus_step(1, false));
            assert_eq!(
                b.read(app).focused_entry().map(|e| e.name),
                Some("a".into())
            );

            for _ in 0..2 {
                b.update(app, |b, _| b.focus_step(1, false));
            }
            assert_eq!(
                b.read(app).focused_entry().map(|e| e.name),
                Some("c".into())
            );

            b.update(app, |b, _| b.focus_step(1, false));
            assert_eq!(
                b.read(app).focused_entry().map(|e| e.name),
                Some("a".into()),
                "focus wraps around the end"
            );

            b.update(app, |b, _| b.focus_step(-1, false));
            assert_eq!(
                b.read(app).focused_entry().map(|e| e.name),
                Some("c".into())
            );
        });
    }

    #[gpui::test]
    async fn focus_step_on_an_empty_listing_does_nothing(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                assert!(!b.focus_step(1, false), "there is nowhere to move to");
                assert!(b.focused_entry().is_none());
            });
        });
    }

    #[gpui::test]
    async fn shift_moving_the_focus_keeps_the_earlier_selection(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", true), entry("b", true), entry("c", true)])
            });

            b.update(app, |b, _| b.click_row(0, Extend::None));
            b.update(app, |b, _| b.focus_step(1, true));
            assert_eq!(b.read(app).selected().len(), 2, "the anchor row is kept");

            b.update(app, |b, _| b.focus_step(1, false));
            assert_eq!(
                b.read(app).selected().len(),
                1,
                "a plain move narrows it again"
            );
        });
    }

    #[gpui::test]
    async fn the_hidden_file_toggle_filters_both_views(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![
                    entry(".hidden", false),
                    entry("shown", false),
                    entry(".also", false),
                ]);
                assert_eq!(b.visible.len(), 1, "dotfiles start hidden, as in Nautilus");

                b.show_hidden = true;
                b.recompute_visible();
                assert_eq!(b.visible.len(), 3);

                b.show_hidden = false;
                b.recompute_visible();
                assert_eq!(b.visible.len(), 1, "and hiding them again works");
            });
        });
    }

    #[gpui::test]
    async fn search_filters_on_the_name_case_insensitively(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![
                    entry("Holiday.png", false),
                    entry("notes.txt", false),
                    entry("a", true),
                ]);

                for (query, expected) in
                    [("holi", 1), ("HOLI", 1), ("NOTES", 1), ("zzz", 0), ("", 3)]
                {
                    b.set_search_query_raw(query.to_string());
                    assert_eq!(b.visible.len(), expected, "query {query:?}");
                }
            });
        });
    }

    #[gpui::test]
    async fn filtering_drops_selections_that_are_no_longer_visible(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("keep", false), entry("drop", false)]);
                // Entries are sorted on the way in, so find the row by name
                // rather than assuming an index.
                let drop = (0..b.entries.len())
                    .find(|ix| b.entries[*ix].name == "drop")
                    .expect("the entry is present");
                b.click_row(drop, Extend::None);
                assert_eq!(b.selected().len(), 1);

                b.set_search_query_raw("keep".into());
                assert!(
                    b.selected().is_empty(),
                    "a row the filter hid must not stay selected, or Delete would act \\
                     on something the user cannot see"
                );
            });
        });
    }

    #[gpui::test]
    async fn breadcrumbs_name_the_source_and_collapse_internal_storage(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("serial123".into());
                b.device_display = "Pixel 7".into();
                b.current_path = PathBuf::from("/");

                let crumbs = b.breadcrumbs();
                assert_eq!(crumbs.len(), 1, "the root is just the device");
                assert_eq!(crumbs[0].0, "Pixel 7");
                assert_eq!(crumbs[0].1, PathBuf::from("/"));

                b.current_path = PathBuf::from("/sdcard/Download");
                let crumbs = b.breadcrumbs();
                assert_eq!(crumbs[0].0, "Pixel 7", "the device names the first crumb");
                assert_eq!(
                    crumbs[1].0, "Internal storage",
                    "/sdcard gets a friendly name"
                );
                assert_eq!(crumbs[1].1, PathBuf::from("/sdcard"));
                assert_eq!(crumbs.last().unwrap().0, "Download");

                b.local_mode = true;
                b.current_path = PathBuf::from("/home/me/Pictures");
                let crumbs = b.breadcrumbs();
                assert_eq!(crumbs[0].0, "This computer");
                assert_eq!(crumbs.last().unwrap().0, "Pictures");
            });
        });
    }

    #[gpui::test]
    async fn breadcrumbs_fall_back_to_a_short_serial(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("R5CT30ABCDE".into());
                b.device_display.clear();
                assert_eq!(b.device_label(), "T30ABCDE");
            });
        });
    }

    #[gpui::test]
    async fn local_paths_come_from_the_fuse_mount_for_device_entries(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("serial".into());
                b.current_path = PathBuf::from("/sdcard/DCIM");
                b.fuse_mount = Some("/run/user/1000/adbshare/serial".into());
                assert_eq!(
                    b.local_path_of(&entry("cat.png", false)),
                    Some(PathBuf::from(
                        "/run/user/1000/adbshare/serial/sdcard/DCIM/cat.png"
                    ))
                );

                b.fuse_mount = None;
                assert_eq!(
                    b.local_path_of(&entry("cat.png", false)),
                    None,
                    "with no mount there is no local path, so external open is skipped"
                );

                b.local_mode = true;
                b.current_path = PathBuf::from("/home/me");
                assert_eq!(
                    b.local_path_of(&entry("a.txt", false)),
                    Some(PathBuf::from("/home/me/a.txt"))
                );
            });
        });
    }

    #[gpui::test]
    async fn navigating_records_where_we_came_from(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("s".into());
                b.current_path = PathBuf::from("/sdcard");

                b.navigate(PathBuf::from("/sdcard/Download"));
                assert!(b.can_go_back());
                assert!(!b.can_go_forward());
                assert_eq!(b.path(), Path::new("/sdcard/Download"));

                b.navigate(PathBuf::from("/sdcard/DCIM"));
                assert_eq!(b.back.len(), 2, "each move extends the back stack");
            });
        });
    }

    #[gpui::test]
    async fn back_and_forward_move_between_visited_directories(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("s".into());
                b.current_path = PathBuf::from("/sdcard");
                b.navigate(PathBuf::from("/sdcard/Download"));
                b.navigate(PathBuf::from("/sdcard/DCIM"));

                assert_eq!(b.go_back(), Some(PathBuf::from("/sdcard/Download")));
                assert_eq!(
                    b.path(),
                    Path::new("/sdcard/Download"),
                    "the move is immediate"
                );
                assert_eq!(b.go_back(), Some(PathBuf::from("/sdcard")));
                assert!(!b.can_go_back(), "the back stack is exhausted");

                // Forward replays the hops in reverse, most recent first.
                assert_eq!(b.go_forward(), Some(PathBuf::from("/sdcard/Download")));
                assert_eq!(b.go_forward(), Some(PathBuf::from("/sdcard/DCIM")));
                assert_eq!(b.path(), Path::new("/sdcard/DCIM"));
                assert!(!b.can_go_forward());
            });
        });
    }

    #[gpui::test]
    async fn going_up_is_unavailable_at_the_root(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.local_mode = true;
                b.current_path = PathBuf::from("/");
                assert!(!b.can_go_up());
                b.current_path = PathBuf::from("/home");
                assert!(b.can_go_up());
            });
        });
    }

    #[gpui::test]
    async fn opening_a_folder_moves_there_and_reports_the_listing_to_fetch(
        cx: &mut TestAppContext,
    ) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("s".into());
                b.current_path = PathBuf::from("/sdcard");
                b.install_entries(vec![entry("DCIM", true), entry("a.txt", false)]);

                let outcome = b.open_index(0).expect("there is an entry");
                assert_eq!(
                    outcome,
                    OpenOutcome::Directory(PathBuf::from("/sdcard/DCIM"))
                );
                assert_eq!(b.path(), Path::new("/sdcard/DCIM"));
                assert!(b.can_go_back(), "the previous directory is remembered");
                assert!(b.entries.is_empty(), "the stale listing is dropped");
            });
        });
    }

    #[gpui::test]
    async fn opening_an_apk_asks_for_an_install_rather_than_navigating(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("s".into());
                b.current_path = PathBuf::from("/sdcard");
                b.install_entries(vec![entry("app.apk", false)]);

                let outcome = b.open_index(0).expect("there is an entry");
                assert!(matches!(outcome, OpenOutcome::InstallApk(_)));
                assert_eq!(
                    b.path(),
                    Path::new("/sdcard"),
                    "an APK must not be treated as a directory"
                );
            });
        });
    }

    #[gpui::test]
    async fn opening_a_plain_file_asks_for_a_download(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("s".into());
                b.current_path = PathBuf::from("/sdcard");
                b.install_entries(vec![entry("notes.txt", false)]);

                let outcome = b.open_index(0).expect("there is an entry");
                assert!(matches!(outcome, OpenOutcome::Download(_)));
                assert_eq!(b.path(), Path::new("/sdcard"));
            });
        });
    }

    #[gpui::test]
    async fn the_focused_entry_falls_back_to_the_selection_then_nowhere(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", false), entry("b", false)]);
                assert!(
                    b.focused_entry().is_none(),
                    "an empty listing has nothing to act on"
                );

                b.click_row(1, Extend::None);
                assert_eq!(b.focused_entry().map(|e| e.name), Some("b".into()));

                b.selection.clear();
                assert_eq!(
                    b.focused_entry().map(|e| e.name),
                    Some("b".into()),
                    "the focused row survives the selection being cleared"
                );

                b.focused = None;
                assert_eq!(
                    b.focused_entry().map(|e| e.name),
                    None,
                    "with neither focus nor selection there is nothing to act on"
                );
            });
        });
    }

    #[gpui::test]
    async fn the_focused_entry_falls_back_to_the_first_selected_row(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", false), entry("b", false)]);
                b.click_row(1, Extend::None);
                b.focused = None;
                assert_eq!(
                    b.focused_entry().map(|e| e.name),
                    Some("b".into()),
                    "with no focused row the selection is used"
                );
            });
        });
    }

    /// The top bar's buttons drive the browser directly, so they need a public
    /// entry point for each. These cover the ones the top bar calls, because
    /// they used to dispatch an action instead — and an action travels the focus
    /// path, which nothing ever entered, so the buttons did nothing at all.
    #[gpui::test]
    async fn the_top_bar_buttons_can_drive_the_browser(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| {
                b.device = Some("s".into());
                b.install_entries(vec![entry("a", false), entry("b", true)]);
                b.navigate(PathBuf::from("/sdcard/Download"));
                cx.notify();
            });
        });

        // The two layout buttons set what they name, so the list button does not
        // switch back to the grid when pressed while already in the list.
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| b.set_view_mode(ViewMode::List, cx));
            assert_eq!(browser.read(app).view_mode(), ViewMode::List);
            browser.update(app, |b, cx| b.set_view_mode(ViewMode::List, cx));
            assert_eq!(
                browser.read(app).view_mode(),
                ViewMode::List,
                "pressing the active layout button must not flip it"
            );
            browser.update(app, |b, cx| b.set_view_mode(ViewMode::Grid, cx));
            assert_eq!(browser.read(app).view_mode(), ViewMode::Grid);
            browser.update(app, |b, cx| b.flip_view_mode(cx));
            assert_eq!(browser.read(app).view_mode(), ViewMode::List);
        });

        // Search toggles both ways and clears the query on close.
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| b.set_search_active(true, cx));
            assert!(browser.read(app).search_active());
            browser.update(app, |b, _cx| b.set_search_query_raw("a".into()));
            browser.update(app, |b, cx| b.set_search_active(false, cx));
            let b = browser.read(app);
            assert!(!b.search_active());
            assert_eq!(b.search_query, "", "closing search clears the query");
        });

        // Navigation moves the browser and asks for the new listing.
        cx.update(|app| {
            let browser = handle.root(app).expect("root view");
            browser.update(app, |b, cx| {
                b.navigate(PathBuf::from("/sdcard/DCIM"));
                b.step_back(cx);
            });
            assert_eq!(
                browser.read(app).path(),
                Path::new("/sdcard/Download"),
                "back returns to the previous directory"
            );

            browser.update(app, |b, cx| b.step_forward(cx));
            assert_eq!(browser.read(app).path(), Path::new("/sdcard/DCIM"));

            // `up` at the root is a no-op rather than an error.
            browser.update(app, |b, cx| {
                b.navigate(PathBuf::from("/"));
                b.step_up(cx);
            });
            assert_eq!(browser.read(app).path(), Path::new("/"));
        });
    }

    /// A tile is a fixed size whatever the file is called.
    ///
    /// The label and the size are centred text in a full-width box: without
    /// `text_center` the box is centred but the text is not, so names sat against
    /// the tile's left edge and a long one ellipsized from the wrong side. What
    /// is checkable here is the invariant that a name cannot widen its tile and
    /// break the grid's rhythm.
    #[gpui::test]
    async fn tile_width_does_not_depend_on_the_file_name(_cx: &mut TestAppContext) {
        // Each name is measured in its own context, so the layouts cannot
        // influence one another.
        let names = [
            ("a", false),
            ("a-rather-longer-name-than-the-tile-is-wide.bin", false),
            ("mid-length.dat", true),
        ];

        let mut widths = Vec::new();
        for (name, is_dir) in names {
            let mut cx = TestAppContext::single();
            cx.update(crate::theme::install);
            let handle = open(&mut cx);
            let vctx = cx.add_empty_window();
            let browser = vctx.update(|_w, cx| handle.root(cx).expect("root view"));
            vctx.update(|_w, cx| {
                browser.update(cx, |b, cx| {
                    b.device = Some("bench".into());
                    b.install_entries(vec![entry(name, is_dir)]);
                    b.set_viewport(0.0, 0.0, 768.0, 560.0, cx);
                    cx.notify();
                });
            });
            let _ = vctx.draw(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(768.), px(560.)),
                |_w, _cx| browser.clone(),
            );
            // The grid's tile is a fixed size whatever the name is, because the
            // label truncates rather than widening the tile.
            cx.update(|app| {
                let height: f32 = browser.read(app).grid_geometry.tile.height.into();
                widths.push(height);
            });
        }

        assert!(
            widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.01),
            "tile height changed with the file name: {widths:?}"
        );
    }

    #[gpui::test]
    async fn both_view_modes_render(cx: &mut TestAppContext) {
        // Regression: the list view used to reach for its own entity from
        // inside `uniform_list`'s processor, which runs *during* this entity's
        // render. GPUI treats that as a double lease and panics, taking the
        // whole app down.
        //
        // The window's own root is deliberately left empty and the browser is
        // drawn through `VisualTestContext::draw`, because that is what
        // actually forces layout; `refresh` alone never runs `render` here.
        cx.update(crate::theme::install);
        let (browser, vctx) = cx.add_window_view(|_w, cx| Browser::new(cx));

        for mode in [ViewMode::Grid, ViewMode::List] {
            vctx.update(|_w, cx| {
                browser.update(cx, |b, cx| {
                    // Without a target the browser renders the onboarding
                    // state and never reaches the item area at all.
                    b.device = Some("test-serial".into());
                    b.device_display = "Test phone".into();
                    b.install_entries(vec![
                        entry("folder", true),
                        entry("notes.txt", false),
                        entry("photo.png", false),
                    ]);
                    b.view_mode = mode;
                    cx.notify();
                });
            });

            // Drawing forces layout, which is what runs `render`.
            let _ = vctx.draw(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(400.), px(400.)),
                |_w, _cx| browser.clone(),
            );
        }
    }

    #[gpui::test]
    async fn deletion_is_recoverable_on_the_disk_and_not_on_a_device(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.device = Some("s".into());
                assert!(
                    !b.delete_is_recoverable(),
                    "a device has no trash, so deletion there is permanent"
                );
                b.local_mode = true;
                assert!(b.delete_is_recoverable(), "the disk has a trash");
            });
        });
    }

    // ── Benchmarks ───────────────────────────────────────────────────────────────
    //
    //   cargo test -p adb-gui --release -- --ignored --nocapture bench
    //
    // The GTK build had no way to measure this, which is part of why the grid ended
    // up building an element for every entry: on a phone with a few thousand files
    // in `DCIM` that is thousands of elements per frame. These give a number to
    // optimise against.

    /// Best-of-N time for one layout pass, in milliseconds.
    ///
    /// The browser is drawn through `VisualTestContext::draw` rather than by
    /// refreshing the window, because that is what actually forces layout: a
    /// refresh alone never runs `render` in the test harness.
    fn bench_layout(mode: ViewMode, count: usize) -> f64 {
        let mut cx = TestAppContext::single();
        cx.update(crate::theme::install);
        let handle = open(&mut cx);
        let vctx = cx.add_empty_window();
        let browser = vctx.update(|_w, cx| handle.root(cx).expect("root view"));

        vctx.update(|_w, cx| {
            browser.update(cx, |b, cx| {
                b.device = Some("bench".into());
                b.device_display = "Bench".into();
                b.install_entries(synthetic_listing(count));
                b.view_mode = mode;
                // A realistic viewport, so the grid measures the windowing path it
                // actually takes at runtime rather than the no-layout fallback.
                b.set_viewport(232.0, 45.0, 768.0, 560.0, cx);
                cx.notify();
            });
        });

        let mut best = f64::MAX;
        for _ in 0..12 {
            let start = std::time::Instant::now();
            let _ = vctx.draw(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(1000.), px(680.)),
                |_w, _cx| browser.clone(),
            );
            best = best.min(start.elapsed().as_secs_f64() * 1000.0);
        }
        best
    }

    /// The search filter and the local sort, both of which used to allocate per
    /// comparison and are on the path of every keystroke and every navigation.
    #[test]
    #[ignore = "benchmark"]
    fn bench_filter_and_sort_cost() {
        for count in [1_000usize, 5_000] {
            let entries = synthetic_listing(count);

            let mut best = f64::MAX;
            for _ in 0..8 {
                let start = std::time::Instant::now();
                for needle in ["i", "im", "img", "img_", "IMG_2"] {
                    let lower = needle.to_lowercase();
                    let hits = entries
                        .iter()
                        .filter(|e| contains_ignore_case(&e.name, &lower))
                        .count();
                    std::hint::black_box(hits);
                }
                best = best.min(start.elapsed().as_secs_f64() * 1000.0);
            }
            println!("bench filter n={count:<6} 5 queries {best:8.3} ms");

            let mut best = f64::MAX;
            for _ in 0..8 {
                let mut copy = entries.clone();
                let start = std::time::Instant::now();
                crate::protocol::sort_entries(&mut copy, crate::protocol::SortKey::Name, false);
                best = best.min(start.elapsed().as_secs_f64() * 1000.0);
                std::hint::black_box(&copy);
            }
            println!("bench sort   n={count:<6} {best:8.3} ms");
        }
    }

    /// How much of the grid's cost is per-tile rather than fixed overhead.
    ///
    /// The window means the visible tile count is driven by the tile size, so
    /// sweeping the zoom sweeps the number of tiles actually built.
    #[test]
    #[ignore = "benchmark"]
    fn bench_grid_cost_by_visible_tile_count() {
        for zoom in [24.0_f32, 48.0, 96.0, 128.0] {
            let mut cx = TestAppContext::single();
            cx.update(crate::theme::install);
            let handle = open(&mut cx);
            let vctx = cx.add_empty_window();
            let browser = vctx.update(|_w, cx| handle.root(cx).expect("root view"));
            vctx.update(|_w, cx| {
                browser.update(cx, |b, cx| {
                    b.device = Some("bench".into());
                    b.install_entries(synthetic_listing(5_000));
                    b.zoom = zoom;
                    b.set_viewport(232.0, 45.0, 768.0, 560.0, cx);
                    cx.notify();
                });
            });

            let tile_w = zoom + GRID_TILE_PAD * 2.0;
            let columns = grid_columns(768.0, tile_w, GRID_GAP);
            let tile_h = tile_w + GRID_TILE_CAPTION + GRID_GAP;
            let rows = (560.0 / tile_h).ceil() as usize + 3;
            let tiles = (rows * columns).min(5_000);

            let mut best = f64::MAX;
            for _ in 0..12 {
                let start = std::time::Instant::now();
                let _ = vctx.draw(
                    gpui::point(px(0.), px(0.)),
                    gpui::size(px(1000.), px(680.)),
                    |_w, _cx| browser.clone(),
                );
                best = best.min(start.elapsed().as_secs_f64() * 1000.0);
            }
            println!(
                "bench grid zoom={zoom:<6} ~{tiles:>3} tiles  {best:8.3} ms  ({:.3} ms/tile)",
                best / tiles as f64
            );
        }
    }

    #[test]
    #[ignore = "benchmark; run with --ignored --nocapture"]
    fn bench_layout_cost_by_listing_size() {
        for count in [200usize, 1_000, 5_000] {
            for (label, mode) in [("grid", ViewMode::Grid), ("list", ViewMode::List)] {
                let ms = bench_layout(mode, count);
                println!("bench {label:<5} n={count:<6} {ms:8.3} ms");
            }
        }
    }
}
