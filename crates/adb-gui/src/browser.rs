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
    AnyElement, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    ScrollHandle, Size, Window, actions, div, px, relative, rgba, uniform_list,
};

use crate::icons::{self, names};
use crate::protocol::{DirEntry, human_size};
use crate::theme::{self, Themed};
use crate::ui;

/// How entries are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    List,
}

/// Grid icon sizes, in logical pixels, for Ctrl+= / Ctrl+-.
const ZOOM_MIN: f32 = 24.0;
const ZOOM_MAX: f32 = 128.0;
const ZOOM_STEP: f32 = 12.0;
const ZOOM_DEFAULT: f32 = 48.0;
/// Padding around a grid tile, on top of the icon.
const GRID_TILE_PAD: f32 = 14.0;
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
        ToggleHidden,
        ZoomIn,
        ZoomOut,
        ToggleView,
        NewFolder,
        RenameFocused,
        DeleteSelection,
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

/// Register the browser's key bindings. Called once from `main`.
pub fn install_key_bindings(cx: &mut gpui::App) {
    // `secondary-` is Ctrl on Linux and Cmd on macOS. Writing `cmd-` here would
    // bind the Super key, which is not what a file manager should use.
    cx.bind_keys([
        KeyBinding::new("secondary-a", SelectAll, None),
        KeyBinding::new("escape", Deselect, None),
        KeyBinding::new("delete", DeleteSelection, None),
        KeyBinding::new("f2", RenameFocused, None),
        KeyBinding::new("alt-left", Back, None),
        KeyBinding::new("alt-right", Forward, None),
        KeyBinding::new("alt-up", Up, None),
        KeyBinding::new("f5", Refresh, None),
        KeyBinding::new("secondary-f", ToggleSearch, None),
        KeyBinding::new("secondary-l", TogglePathEntry, None),
        KeyBinding::new("secondary-shift-c", CopySelection, None),
        KeyBinding::new("secondary-c", CopySelection, None),
        KeyBinding::new("secondary-v", Paste, None),
        KeyBinding::new("secondary-u", Paste, None),
        KeyBinding::new("secondary-equal", ZoomIn, None),
        KeyBinding::new("secondary-plus", ZoomIn, None),
        KeyBinding::new("secondary-minus", ZoomOut, None),
        KeyBinding::new("alt-t", ToggleView, None),
        KeyBinding::new("up", FocusPrevious, None),
        KeyBinding::new("down", FocusNext, None),
        KeyBinding::new("home", FocusFirst, None),
        KeyBinding::new("end", FocusLast, None),
    ]);
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
    zoom: f32,
    search_query: String,
    search_active: bool,
    path_entry_active: bool,
    scroll: ScrollHandle,
    grid_geometry: GridGeometry,
    list_geometry: ListGeometry,

    // ── Item-area metrics ───────────────────────────────────────────────────
    //
    // `render` cannot ask the compositor for its own bounds, so the app root
    // reports where the file area is laid out. The rubber-band selection then
    // hit-tests against exact geometry instead of guessing.
    viewport_x: f32,
    viewport_y: f32,
    viewport_width: f32,
    viewport_height: f32,

    // ── Rubber-band selection ──────────────────────────────────────────────
    drag_anchor: Option<Point<Pixels>>,
    drag_current: Option<Point<Pixels>>,
    /// Whether the current drag extends the selection rather than replacing it.
    drag_extend: bool,

    // ── Status line ────────────────────────────────────────────────────────
    status_text: String,
    active_jobs: usize,
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
            zoom: ZOOM_DEFAULT,
            search_query: String::new(),
            search_active: false,
            path_entry_active: false,
            scroll: ScrollHandle::new(),
            grid_geometry: GridGeometry::default(),
            list_geometry: ListGeometry::default(),
            viewport_x: 0.0,
            viewport_y: 0.0,
            viewport_width: 0.0,
            viewport_height: 0.0,
            drag_anchor: None,
            drag_current: None,
            drag_extend: false,
            status_text: String::new(),
            active_jobs: 0,
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

    /// Switch between the grid and list layouts.
    pub fn toggle_view_mode(&mut self) {
        self.view_mode = match self.view_mode {
            ViewMode::Grid => ViewMode::List,
            ViewMode::List => ViewMode::Grid,
        };
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
    pub fn install_entries(&mut self, entries: Vec<DirEntry>) {
        self.entries = entries;
        self.selection.clear();
        self.focused = None;
        self.recompute_visible();
    }

    /// Report a listing failure in the status bar.
    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.entries.clear();
        self.selection.clear();
        self.visible.clear();
        self.status_text = message;
        cx.notify();
    }

    /// Update the transfer counters shown in the status bar.
    pub fn set_job_counts(
        &mut self,
        active: usize,
        total: usize,
        paused: bool,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self.active_jobs == active && self.transfers_paused == paused;
        self.active_jobs = active;
        self.transfers_paused = paused;
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
    }

    // ── State transforms, kept free of `Context` so they can be tested ─────

    /// A plain click replaces the selection; Shift or Ctrl toggles the row in or
    /// out of it.
    pub fn click_row(&mut self, index: usize, extend: bool) -> bool {
        if extend {
            if !self.selection.insert(index) {
                self.selection.remove(&index);
            }
        } else {
            self.selection.clear();
            self.selection.insert(index);
        }
        self.focused = Some(index);
        true
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
            .filter(|(_, entry)| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .map(|(ix, _)| ix)
            .collect();
        // Drop selections the filter just hid, so Copy and Delete cannot act on
        // something the user can no longer see.
        self.selection.retain(|ix| self.visible.contains(ix));
        if self.focused.is_some_and(|ix| !self.visible.contains(&ix)) {
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
        self.toggle_view_mode();
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

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.can_go_up() {
            cx.emit(BrowserEvent::Up);
        }
    }

    fn back(&mut self, _: &Back, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.go_back() {
            cx.notify();
            cx.emit(BrowserEvent::OpenedDirectory(target));
        }
    }

    fn forward(&mut self, _: &Forward, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.go_forward() {
            cx.notify();
            cx.emit(BrowserEvent::OpenedDirectory(target));
        }
    }

    fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
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
        self.search_active = !self.search_active;
        if !self.search_active && !self.search_query.is_empty() {
            self.set_search_query_raw(String::new());
        }
        cx.notify();
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

    /// Which grid rows to build this frame, and how many.
    ///
    /// Reads the scroll offset recorded by the last frame's layout, so the
    /// window is at most one frame stale. A row of overscan on each side keeps
    /// fast scrolling from showing gaps.
    fn visible_row_range(&self, tile_h: f32) -> (usize, usize) {
        const OVERSCAN: usize = 1;
        let viewport_h: f32 = self.viewport_height;
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
        cx.notify();
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
        if !self.drag_extend {
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
pub fn step_zoom(current: f32, delta: f32) -> f32 {
    (current + delta).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// Number of grid tiles that fit across `container_w`.
pub fn grid_columns(container_w: f32, tile_w: f32, gap: f32) -> usize {
    if tile_w <= 0.0 || container_w <= 0.0 {
        return 1;
    }
    // The `+ 1.0` accounts for the trailing gap, which the last tile does not
    // have, so a row that exactly fits is not undercounted.
    (((container_w + gap) / (tile_w + gap) + 1.0).floor() as usize).max(1)
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
        .bg(t.surface_raised)
        .border_1()
        .border_color(rgba(0xFFFFFF0D))
        .child(icons::icon(icon, 16.0, t.text_header))
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
        .child(icons::icon(names::PHONE, 40.0, t.text_muted))
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
fn status_bar(browser: &Browser, t: &theme::Palette, owner: &Entity<Browser>) -> AnyElement {
    let running = browser.active_jobs > 0;
    let text = if browser.status_text.is_empty() {
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
                        "Transfers paused"
                    } else {
                        "Transferring"
                    }),
            )
        })
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
                    .hover(|s| s.bg(t.hover_strong))
                    .child(icons::icon(
                        if browser.transfers_paused {
                            names::PLAY
                        } else {
                            names::PAUSE
                        },
                        12.0,
                        t.text_dim,
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
                    .hover(|s| s.bg(t.hover_strong))
                    .child(icons::icon(names::STOP, 12.0, t.text_dim))
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
            .bg(rgba(0xFFFFFF14))
            .into_any_element(),
    )
}

/// The folder glyph for a directory, chosen by name where that reads better than
/// a plain folder.
fn folder_icon_for(entry: &DirEntry) -> &'static str {
    let lower = entry.name.to_lowercase();
    if lower == "documents" || lower == "document" {
        names::FOLDER_DOCUMENTS
    } else if lower == "downloads" || lower == "download" {
        names::FOLDER_DOWNLOAD
    } else if lower == "music" {
        names::FOLDER_MUSIC
    } else if lower == "videos" || lower == "movies" || lower == "video" {
        names::FOLDER_VIDEOS
    } else if lower.contains("screenshot")
        || lower.contains("camera")
        || lower == "dcim"
        || lower == "pictures"
    {
        names::FOLDER_PICTURES
    } else {
        names::FOLDER
    }
}

/// Full-colour artwork for the file types the design has bespoke art for, or
/// `None` to fall back to a monochrome glyph.
fn artwork_for(entry: &DirEntry) -> Option<&'static str> {
    Some(match entry.ext().as_str() {
        "apk" => "apk",
        "zip" | "7z" | "rar" => "zip",
        "tar" | "gz" | "tgz" | "xz" => "tar",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odg"
        | "csv" | "epub" => "documents",
        "mp3" | "flac" | "ogg" | "wav" | "m4a" | "aac" => "podcasts",
        "txt" | "log" | "json" | "xml" => "txt",
        "mp4" | "mkv" | "avi" | "webm" | "mov" => "movie",
        "png" | "jpg" | "jpeg" | "webp" | "gif" => "wallpaper",
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
        let extend = event.modifiers().shift || event.modifiers().control;
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
            self.click_row(index, false);
        }
        self.focused = Some(index);
        cx.notify();
        cx.emit(BrowserEvent::ContextMenu {
            position: event.position,
            focused: Some(index),
        });
    }

    /// The monochrome glyph for an entry.
    fn glyph_for(&self, entry: &DirEntry) -> &'static str {
        if entry.looks_like_dir() {
            return folder_icon_for(entry);
        }
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
        let tile_h = tile_w + 34.0;

        let icon_el: AnyElement = match artwork_for(&entry) {
            Some(art) if !entry.looks_like_dir() => {
                icons::artwork(art, icon_size).into_any_element()
            }
            _ => icons::icon(
                self.glyph_for(&entry),
                if entry.looks_like_dir() {
                    icon_size
                } else {
                    icon_size * 0.9
                },
                if selected { t.text_header } else { t.text_dim },
            )
            .into_any_element(),
        };

        let label = div()
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

        let sub = ui::mono(
            if entry.is_dir {
                "—".to_string()
            } else {
                human_size(entry.size)
            },
            &t,
        )
        .text_size(px(9.5));

        Some(
            div()
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
                .when(selected, |d| d.bg(t.hover_strong))
                .when(focused && !selected, |d| d.bg(t.hover))
                .hover(|d| d.bg(if selected { t.pressed } else { t.hover }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(icon_size))
                        .w_full()
                        .child(icon_el),
                )
                .child(div().mt(px(6.0)).w_full().px(px(2.0)).child(label))
                .child(div().mt(px(1.0)).child(sub))
                .on_click(
                    cx.listener(move |this, event, w, cx| this.on_item_click(index, event, w, cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event, w, cx| {
                        this.on_item_right_click(index, event, w, cx)
                    }),
                )
                .into_any_element(),
        )
    }

    /// One row of the list view.
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

        Some(
            div()
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
                .when(selected, |d| d.bg(t.hover_strong))
                .when(focused && !selected, |d| d.bg(t.hover))
                .hover(|d| d.bg(if selected { t.pressed } else { t.hover }))
                .child(icons::icon(
                    self.glyph_for(&entry),
                    15.0,
                    if selected { t.text_header } else { t.text_dim },
                ))
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
                )
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

        // Work out the item geometry now, from the metrics the app root reports,
        // so a drag can hit-test without asking the compositor for bounds.
        let tile_w = self.zoom + GRID_TILE_PAD * 2.0;
        self.grid_geometry = GridGeometry {
            origin: gpui::point(px(self.viewport_x), px(self.viewport_y)),
            tile: Size {
                width: px(tile_w),
                height: px(tile_w + 34.0),
            },
            columns: grid_columns(self.viewport_width, tile_w, GRID_GAP),
        };
        self.list_geometry = ListGeometry {
            origin_y: px(self.viewport_y),
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
                    let tile_w = self.zoom + GRID_TILE_PAD * 2.0;
                    let tile_h = tile_w + 34.0 + GRID_GAP;
                    let columns = self.grid_geometry.columns.max(1);
                    let rows = count.div_ceil(columns);

                    // Only build the rows that can be on screen. A phone's DCIM
                    // runs to thousands of files, and building a tile for each
                    // one every frame cost ~160ms at 5,000 entries; windowing
                    // keeps this proportional to the viewport instead.
                    let (first_row, visible_rows) = self.visible_row_range(tile_h);
                    let total_height = rows as f32 * tile_h + GRID_TILE_PAD;

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
            .on_action(cx.listener(Self::toggle_hidden))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::toggle_view))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::rename_focused))
            .on_action(cx.listener(Self::delete_selection))
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
            let tile_h = b.zoom + GRID_TILE_PAD * 2.0 + 34.0 + GRID_GAP;
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
            let tile_h = b.zoom + GRID_TILE_PAD * 2.0 + 34.0 + GRID_GAP;
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
                let tile_w = b.zoom + GRID_TILE_PAD * 2.0;
                let height = rows as f32 * (tile_w + 34.0 + GRID_GAP) + GRID_TILE_PAD;
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
        let mut zoom = ZOOM_DEFAULT;
        for _ in 0..40 {
            zoom = step_zoom(zoom, ZOOM_STEP);
        }
        assert_eq!(zoom, ZOOM_MAX, "zoom in stops at the maximum");
        for _ in 0..60 {
            zoom = step_zoom(zoom, -ZOOM_STEP);
        }
        assert_eq!(zoom, ZOOM_MIN, "zoom out stops at the minimum");
        assert_eq!(
            step_zoom(zoom, ZOOM_STEP),
            ZOOM_MIN + ZOOM_STEP,
            "and it can come back"
        );
    }

    #[test]
    fn grid_columns_accounts_for_gaps_and_degenerate_input() {
        assert_eq!(grid_columns(500.0, 100.0, GRID_GAP), 5);
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
    fn folders_get_a_variant_glyph_by_name() {
        assert_eq!(
            folder_icon_for(&entry("Documents", true)),
            names::FOLDER_DOCUMENTS
        );
        assert_eq!(
            folder_icon_for(&entry("Downloads", true)),
            names::FOLDER_DOWNLOAD
        );
        assert_eq!(folder_icon_for(&entry("Music", true)), names::FOLDER_MUSIC);
        assert_eq!(
            folder_icon_for(&entry("Videos", true)),
            names::FOLDER_VIDEOS
        );
        assert_eq!(
            folder_icon_for(&entry("DCIM", true)),
            names::FOLDER_PICTURES
        );
        assert_eq!(
            folder_icon_for(&entry("Screenshots", true)),
            names::FOLDER_PICTURES
        );
        assert_eq!(folder_icon_for(&entry("random", true)), names::FOLDER);
    }

    #[test]
    fn artwork_covers_the_types_the_design_has_art_for() {
        for (name, art) in [
            ("a.apk", "apk"),
            ("a.zip", "zip"),
            ("a.7z", "zip"),
            ("a.tar.gz", "tar"),
            ("a.pdf", "documents"),
            ("a.epub", "documents"),
            ("a.mp3", "podcasts"),
            ("a.txt", "txt"),
            ("a.json", "txt"),
            ("a.mp4", "movie"),
            ("a.png", "wallpaper"),
        ] {
            assert_eq!(artwork_for(&entry(name, false)), Some(art), "{name}");
        }
        assert_eq!(artwork_for(&entry("noextension", false)), None);
        assert_eq!(artwork_for(&entry("a.bin", false)), None);
    }

    #[test]
    fn every_referenced_asset_is_embedded() {
        let mut referenced: Vec<String> = [
            folder_icon_for(&entry("x", true)),
            names::PHONE,
            names::DRIVE_HARDDISK,
            names::FOLDER,
            names::TEXT_GENERIC,
            names::IMAGE_GENERIC,
            names::AUDIO_GENERIC,
            names::VIDEO_GENERIC,
            names::PACKAGE,
            names::DOCUMENT,
        ]
        .into_iter()
        .map(|name| format!("icons/{name}.svg"))
        .collect();
        for art in [
            "apk",
            "zip",
            "tar",
            "documents",
            "podcasts",
            "txt",
            "movie",
            "wallpaper",
        ] {
            referenced.push(format!("art/{art}.svg"));
        }
        referenced.sort();
        referenced.dedup();

        for path in referenced {
            assert!(
                crate::icons::ASSETS.iter().any(|(asset, _)| *asset == path),
                "{path} is referenced by the browser but not embedded, so it would \\
                 render as nothing"
            );
        }
    }

    // ── State: these need a real entity, hence the harness ─────────────────

    #[gpui::test]
    async fn clicking_replaces_the_selection_and_shift_extends_it(cx: &mut TestAppContext) {
        let handle = open(cx);
        cx.update(|app| {
            let b = handle.root(app).expect("root view");
            b.update(app, |b, _| {
                b.install_entries(vec![entry("a", true), entry("b", true), entry("c", false)])
            });

            b.update(app, |b, _| b.click_row(0, false));
            assert_eq!(b.read(app).selected().len(), 1);

            b.update(app, |b, _| b.click_row(2, false));
            assert_eq!(
                b.read(app).selected().len(),
                1,
                "a plain click replaces the selection"
            );
            assert_eq!(b.read(app).selected()[0].name, "c");

            b.update(app, |b, _| b.click_row(0, true));
            assert_eq!(
                b.read(app).selected().len(),
                2,
                "shift adds to the selection"
            );

            b.update(app, |b, _| b.click_row(0, true));
            assert_eq!(
                b.read(app).selected().len(),
                1,
                "shift on an already-selected row removes it"
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

            b.update(app, |b, _| b.click_row(0, false));
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
                b.click_row(1, false);
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

                b.click_row(1, false);
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
                b.click_row(1, false);
                b.focused = None;
                assert_eq!(
                    b.focused_entry().map(|e| e.name),
                    Some("b".into()),
                    "with no focused row the selection is used"
                );
            });
        });
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

    /// A listing with a realistic mix of Android media and app files.
    fn synthetic_listing(count: usize) -> Vec<DirEntry> {
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
