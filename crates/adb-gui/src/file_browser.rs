//! Nautilus-style file browser with interactive breadcrumb path bar,
//! navigation history (back/forward/up/refresh), live search filtering,
//! MIME file type icons, context menus, Grid/List view switcher,
//! ADB status banner, and file operations.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
}

/// Bundled full-color icons matching the stitch design (Nautilus-style folders,
/// APK android, archives, ...). Extracted to a temp dir at startup because
/// GdkPixbuf loads SVGs from files.
const ASSET_ICONS: &[(&str, &[u8])] = &[
    ("folder.png", include_bytes!("assets/folder.png")),
    ("folder-documents.png", include_bytes!("assets/folder-documents.png")),
    ("folder-download.png", include_bytes!("assets/folder-download.png")),
    ("folder-music.png", include_bytes!("assets/folder-music.png")),
    ("folder-pictures.png", include_bytes!("assets/folder-pictures.png")),
    ("folder-videos.png", include_bytes!("assets/folder-videos.png")),
    ("camera.svg", include_bytes!("assets/camera.svg")),
    ("documents.svg", include_bytes!("assets/documents.svg")),
    ("magisk.svg", include_bytes!("assets/magisk.svg")),
    ("podcasts.svg", include_bytes!("assets/podcasts.svg")),
    ("telegram.svg", include_bytes!("assets/telegram.svg")),
    ("screenshots.svg", include_bytes!("assets/screenshots.svg")),
    ("zip.svg", include_bytes!("assets/zip.svg")),
    ("apk.svg", include_bytes!("assets/apk.svg")),
    ("tar.svg", include_bytes!("assets/tar.svg")),
    ("txt.svg", include_bytes!("assets/txt.svg")),
    ("movie.svg", include_bytes!("assets/movie.svg")),
    ("wallpaper.svg", include_bytes!("assets/wallpaper.svg")),
];

/// Extract bundled SVG icons once and return the directory they live in.
fn asset_icon_dir() -> Option<std::path::PathBuf> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("adbshare-icons");
        std::fs::create_dir_all(&dir).ok()?;
        for (name, bytes) in ASSET_ICONS {
            let path = dir.join(name);
            if !path.exists() {
                std::fs::write(&path, bytes).ok()?;
            }
        }
        Some(dir)
    })
    .clone()
}

/// Returns the bundled icon name for an entry, if the design defines one.
fn asset_icon_for(entry: &DirEntry) -> Option<&'static str> {
    if entry.is_dir {
        let lower = entry.name.to_lowercase();
        return Some(if lower == "documents" || lower == "document" {
            "folder-documents.png"
        } else if lower == "downloads" || lower == "download" {
            "folder-download.png"
        } else if lower == "music" {
            "folder-music.png"
        } else if lower.contains("screenshot") || lower.contains("camera") || lower == "dcim" || lower == "pictures" {
            "folder-pictures.png"
        } else if lower == "videos" || lower == "movies" || lower == "video" {
            "folder-videos.png"
        } else {
            "folder.png"
        });
    }
    let lower = entry.name.to_lowercase();
    if lower.ends_with(".apk") {
        Some("apk.svg")
    } else if lower.ends_with(".zip") || lower.ends_with(".7z") || lower.ends_with(".rar") {
        Some("zip.svg")
    } else if lower.ends_with(".tar") || lower.ends_with(".gz") || lower.ends_with(".tgz") || lower.ends_with(".xz") {
        Some("tar.svg")
    } else if lower.ends_with(".txt") || lower.ends_with(".log") || lower.ends_with(".json") || lower.ends_with(".xml") {
        Some("txt.svg")
    } else if lower.ends_with(".mp4") || lower.ends_with(".mkv") || lower.ends_with(".avi") || lower.ends_with(".webm") || lower.ends_with(".mov") {
        Some("movie.svg")
    } else if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".webp") || lower.ends_with(".gif") {
        Some("wallpaper.svg")
    } else {
        None
    }
}

/// Selected entries in the list view (indexes map to the entries vec).
fn selected_entries_from_list(list_box: &gtk4::ListBox, entries: &Rc<RefCell<Vec<DirEntry>>>) -> Vec<DirEntry> {
    let entries = entries.borrow();
    list_box
        .selected_rows()
        .iter()
        .filter_map(|r| {
            let name = r.widget_name().to_string();
            entries.iter().find(|e| e.name == name).cloned()
        })
        .collect()
}

/// Selected entries in the grid view (cards carry their entry name).
fn selected_entries_from_grid(grid_box: &gtk4::FlowBox, entries: &Rc<RefCell<Vec<DirEntry>>>) -> Vec<DirEntry> {
    let entries = entries.borrow();
    grid_box
        .selected_children()
        .iter()
        .filter_map(|c| {
            let name = c.child()?.widget_name().to_string();
            entries.iter().find(|e| e.name == name).cloned()
        })
        .collect()
}

impl DirEntry {
    pub fn display_size(&self) -> String {
        if self.is_dir { return "Folder".to_string(); }
        let mut s = self.size as f64;
        for unit in ["B", "KB", "MB", "GB", "TB"] {
            if s < 1024.0 {
                return if unit == "B" { format!("{} {}", s as u64, unit) }
                        else { format!("{:.1} {}", s, unit) };
            }
            s /= 1024.0;
        }
        format!("{:.1} PB", s)
    }

    pub fn display_date(&self) -> String {
        if self.mtime <= 0 { return String::new(); }
        let days = self.mtime / 86400;
        let secs = (self.mtime % 86400).abs() as u32;
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        let (y, m, d) = days_to_ymd(days);
        format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, hours, mins)
    }

    pub fn icon_name(&self) -> &'static str {
        if self.is_dir {
            return "folder-symbolic";
        }
        let lower = self.name.to_lowercase();
        if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
            || lower.ends_with(".webp") || lower.ends_with(".gif") || lower.ends_with(".svg") {
            "image-x-generic-symbolic"
        } else if lower.ends_with(".mp4") || lower.ends_with(".mkv") || lower.ends_with(".avi")
            || lower.ends_with(".webm") || lower.ends_with(".mov") {
            "video-x-generic-symbolic"
        } else if lower.ends_with(".mp3") || lower.ends_with(".flac") || lower.ends_with(".ogg")
            || lower.ends_with(".wav") || lower.ends_with(".m4a") || lower.ends_with(".aac") {
            "audio-x-generic-symbolic"
        } else if lower.ends_with(".pdf") || lower.ends_with(".doc") || lower.ends_with(".docx")
            || lower.ends_with(".epub") {
            "x-office-document-symbolic"
        } else if lower.ends_with(".apk") {
            "application-x-executable-symbolic"
        } else if lower.ends_with(".zip") || lower.ends_with(".tar") || lower.ends_with(".gz")
            || lower.ends_with(".7z") || lower.ends_with(".rar") {
            "package-x-generic-symbolic"
        } else if lower.ends_with(".xml") || lower.ends_with(".json") || lower.ends_with(".txt")
            || lower.ends_with(".log") || lower.ends_with(".rs") || lower.ends_with(".py") || lower.ends_with(".sh") {
            "text-x-generic-symbolic"
        } else if self.is_symlink {
            "emblem-symbolic-link-symbolic"
        } else {
            "text-x-generic-symbolic"
        }
    }

    pub fn file_type_desc(&self) -> &'static str {
        if self.is_dir { return "Folder"; }
        let lower = self.name.to_lowercase();
        if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".webp") {
            "Image"
        } else if lower.ends_with(".mp4") || lower.ends_with(".mkv") || lower.ends_with(".webm") {
            "Video"
        } else if lower.ends_with(".mp3") || lower.ends_with(".flac") || lower.ends_with(".wav") {
            "Audio"
        } else if lower.ends_with(".pdf") {
            "PDF Document"
        } else if lower.ends_with(".apk") {
            "Android Package (APK)"
        } else if lower.ends_with(".zip") || lower.ends_with(".tar") || lower.ends_with(".gz") {
            "Archive"
        } else {
            "Document"
        }
    }
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 14245) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    List,
}

/// What the browser emits to the app.
#[derive(Debug, Clone)]
pub enum BrowserEvent {
    /// User clicked a directory entry to navigate into it.
    OpenDir(DirEntry),
    /// User navigated to a path (e.g. clicked a breadcrumb or typed a path).
    Navigate(PathBuf),
    /// Selection changed.
    Selected(Option<DirEntry>),
    /// Navigation controls.
    Up,
    Back,
    Forward,
    Refresh,
    /// Actions.
    Upload,
    /// Pull/copy the selected files (multi-select aware).
    Download(Vec<DirEntry>),
    NewFolder(String),
    Rename(DirEntry, String),
    /// Delete the selected items (multi-select aware).
    Delete(Vec<DirEntry>),
    OpenExternal(PathBuf),
    InstallApk(DirEntry),
    OpenTerminal(PathBuf),
    PauseTransfer,
    CancelTransfer,
    /// Files dropped onto `target_dir` (a folder item or the canvas).
    /// `from_dir` is the directory being browsed when the drag started, so
    /// the app can tell internal moves (rename) from external copies.
    DropFiles {
        from_dir: PathBuf,
        target_dir: PathBuf,
        files: Vec<PathBuf>,
    },
}

#[derive(Clone)]
pub struct FileBrowser {
    pub root: gtk4::Box,
    pub list_box: gtk4::ListBox,
    pub grid_box: gtk4::FlowBox,
    pub file_view_stack: gtk4::Stack,
    pub grid_overlay: gtk4::Overlay,
    pub main_stack: gtk4::Stack,
    pub view_mode: Rc<RefCell<ViewMode>>,
    pub back_button: gtk4::Button,
    pub forward_button: gtk4::Button,
    pub up_button: gtk4::Button,
    pub refresh_button: gtk4::Button,
    pub upload_button: gtk4::Button,
    pub download_button: gtk4::Button,
    pub new_folder_button: gtk4::Button,
    pub open_external_button: gtk4::Button,
    pub breadcrumb_container: gtk4::Box,
    pub path_entry: gtk4::Entry,
    pub path_stack: gtk4::Stack,
    pub path_edit_toggle: gtk4::ToggleButton,
    pub search_bar: gtk4::SearchBar,
    pub search_entry: gtk4::SearchEntry,
    pub search_button: gtk4::ToggleButton,

    // Banner widgets
    pub banner_box: gtk4::Box,
    pub banner_status_label: gtk4::Label,
    pub banner_transport_label: gtk4::Label,
    pub banner_battery_label: gtk4::Label,
    pub banner_progress: gtk4::ProgressBar,
    pub banner_pause_btn: gtk4::Button,
    pub banner_cancel_btn: gtk4::Button,

    // Subheader widgets
    pub sub_header_count_label: gtk4::Label,
    pub sub_header_path_label: gtk4::Label,

    current_path: Rc<RefCell<PathBuf>>,
    device: Rc<RefCell<Option<String>>>,
    /// True while browsing the local Linux filesystem instead of a device.
    local_mode: Rc<RefCell<bool>>,
    /// FUSE mountpoint of the selected device (for drag & drop out of the
    /// app); None when unmounted or in local mode.
    fuse_mount: Rc<RefCell<Option<String>>>,
    /// Whether dotfiles are shown (default hidden, like Nautilus).
    show_hidden: Rc<RefCell<bool>>,
    /// Grid icon size in px (zoom: Ctrl+= / Ctrl+-).
    zoom: Rc<Cell<u32>>,
    entries: Rc<RefCell<Vec<DirEntry>>>,
    history_back: Rc<RefCell<Vec<PathBuf>>>,
    history_forward: Rc<RefCell<Vec<PathBuf>>>,
    search_query: Rc<RefCell<String>>,
    on_event: Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>>,
}

impl Default for FileBrowser {
    fn default() -> Self { Self::new() }
}

impl FileBrowser {
    pub fn new() -> Self {
        // Extract bundled icons up front so the first listing never races it.
        let _ = asset_icon_dir();

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.add_css_class("file-canvas");

        // --- ADB Connected Banner (hidden until a device is actually selected) ---
        let banner_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        banner_box.add_css_class("adb-connected-banner");

        let banner_top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        banner_top.set_valign(gtk4::Align::Center);

        let usb_icon = gtk4::Image::from_icon_name("drive-removable-media-symbolic");
        usb_icon.add_css_class("sidebar-icon-places");
        usb_icon.set_pixel_size(18);
        banner_top.append(&usb_icon);

        let banner_title = gtk4::Label::builder()
            .label("Link active")
            .build();
        banner_title.add_css_class("heading");
        banner_top.append(&banner_title);

        let banner_transport_label = gtk4::Label::new(None);
        banner_transport_label.add_css_class("sub-header-path");
        banner_transport_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        banner_top.append(&banner_transport_label);

        let banner_status_label = gtk4::Label::builder()
            .label("Ready")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        banner_status_label.add_css_class("adb-speed-label");
        banner_top.append(&banner_status_label);

        let banner_battery_label = gtk4::Label::new(None);
        banner_battery_label.add_css_class("adb-pill-tag");
        banner_battery_label.set_visible(false);
        banner_top.append(&banner_battery_label);

        let banner_pause_btn = gtk4::Button::builder()
            .label("Pause")
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text("Pause Transfer")
            .sensitive(false)
            .visible(false)
            .build();
        banner_pause_btn.add_css_class("raised-btn");
        banner_top.append(&banner_pause_btn);

        let banner_cancel_btn = gtk4::Button::builder()
            .label("Cancel")
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel Transfer")
            .sensitive(false)
            .visible(false)
            .build();
        banner_cancel_btn.add_css_class("raised-btn");
        banner_cancel_btn.add_css_class("destructive-hover");
        banner_top.append(&banner_cancel_btn);

        banner_box.append(&banner_top);

        let banner_progress = gtk4::ProgressBar::new();
        banner_progress.add_css_class("progress-slim");
        banner_progress.set_visible(false);
        banner_box.append(&banner_progress);

        banner_box.set_visible(false);
        root.append(&banner_box);

        // --- Sub-header Strip (File count & ADB Permissions) ---
        let sub_header_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        sub_header_box.add_css_class("sub-header-bar");

        let sub_header_count_label = gtk4::Label::builder()
            .label("Files & Folders (0 items)")
            .build();
        sub_header_count_label.add_css_class("sub-header-title");
        sub_header_box.append(&sub_header_count_label);

        let sub_sep = gtk4::Label::new(Some("/"));
        sub_sep.add_css_class("nav-pill-sep");
        sub_header_box.append(&sub_sep);

        let sub_header_path_label = gtk4::Label::builder()
            .label("device:none:/")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        sub_header_path_label.add_css_class("sub-header-path");
        sub_header_box.append(&sub_header_path_label);

        root.append(&sub_header_box);

        // --- Search (entry lives in the toolbar; see app.rs) ---
        let search_bar = gtk4::SearchBar::new();
        let search_entry = gtk4::SearchEntry::new();
        search_entry.set_placeholder_text(Some("Search files and folders..."));
        search_bar.connect_entry(&search_entry);

        // --- Navigation Controls (for headerbar / actions) ---
        let back_button = gtk4::Button::from_icon_name("go-previous-symbolic");
        back_button.set_tooltip_text(Some("Back (Alt+Left)"));
        back_button.set_sensitive(false);

        let forward_button = gtk4::Button::from_icon_name("go-next-symbolic");
        forward_button.set_tooltip_text(Some("Forward (Alt+Right)"));
        forward_button.set_sensitive(false);

        let up_button = gtk4::Button::from_icon_name("go-up-symbolic");
        up_button.set_tooltip_text(Some("Parent Folder (Alt+Up)"));
        up_button.set_sensitive(false);

        let path_stack = gtk4::Stack::new();
        path_stack.set_hexpand(true);
        path_stack.set_transition_type(gtk4::StackTransitionType::Crossfade);

        let breadcrumb_scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .build();
        // Small explicit minimum: a deep path must never inflate the header's
        // minimum width (which would clip the window in narrow tiles). Natural
        // width is propagated so the centered crumbs render fully; overflow
        // still scrolls horizontally on narrow windows.
        breadcrumb_scroll.set_width_request(120);
        breadcrumb_scroll.set_propagate_natural_width(true);
        breadcrumb_scroll.set_propagate_natural_height(true);
        let breadcrumb_container = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        breadcrumb_container.set_valign(gtk4::Align::Center);
        // Center the crumbs when the window is wide; the scroll window above
        // clamps the minimum so a deep path scrolls instead of overflowing.
        breadcrumb_container.set_halign(gtk4::Align::Center);
        breadcrumb_scroll.set_child(Some(&breadcrumb_container));
        path_stack.add_named(&breadcrumb_scroll, Some("breadcrumbs"));

        let path_entry = gtk4::Entry::new();
        path_entry.set_placeholder_text(Some("Enter path e.g. /sdcard/Download"));
        path_stack.add_named(&path_entry, Some("entry"));
        path_stack.set_visible_child_name("breadcrumbs");

        let path_edit_toggle = gtk4::ToggleButton::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Toggle Path Entry (Ctrl+L)")
            .build();

        let search_button = gtk4::ToggleButton::builder()
            .icon_name("edit-find-symbolic")
            .tooltip_text("Search files (Ctrl+F)")
            .build();

        let new_folder_button = gtk4::Button::from_icon_name("folder-new-symbolic");
        new_folder_button.set_tooltip_text(Some("New Folder"));

        let upload_button = gtk4::Button::from_icon_name("list-add-symbolic");
        upload_button.set_tooltip_text(Some("Upload files to this folder (ADB Push)"));

        let download_button = gtk4::Button::from_icon_name("folder-download-symbolic");
        download_button.set_tooltip_text(Some("Download selected file (ADB Pull)"));
        download_button.set_sensitive(false);

        let open_external_button = gtk4::Button::from_icon_name("system-file-manager-symbolic");
        open_external_button.set_tooltip_text(Some("Open in Nautilus / File Manager"));

        let refresh_button = gtk4::Button::from_icon_name("view-refresh-symbolic");
        refresh_button.set_tooltip_text(Some("Reload (F5)"));

        // --- File View Stack (Grid View & List View) ---
        let grid_box = gtk4::FlowBox::new();
        // MULTIPLE enables Ctrl+click, Shift+click and rubber-band selection.
        grid_box.set_selection_mode(gtk4::SelectionMode::Multiple);
        grid_box.set_activate_on_single_click(false);
        grid_box.set_homogeneous(true);
        grid_box.set_column_spacing(10);
        grid_box.set_row_spacing(10);
        // Let the row fill the viewport: many narrow columns on wide
        // windows, fewer on narrow ones. min 1 (not 3) so a half-tiled
        // window reflows to fewer columns instead of clipping the last one.
        grid_box.set_max_children_per_line(24);
        grid_box.set_min_children_per_line(1);
        grid_box.set_valign(gtk4::Align::Start);
        grid_box.set_margin_start(12);
        grid_box.set_margin_end(12);
        grid_box.set_margin_top(10);
        grid_box.set_margin_bottom(12);

        let grid_scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();
        grid_scroll.set_child(Some(&grid_box));

        // Overlay hosts the rubber-band selection rectangle.
        let grid_overlay = gtk4::Overlay::new();
        grid_overlay.set_child(Some(&grid_scroll));

        let list_box = gtk4::ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Multiple);
        list_box.set_activate_on_single_click(false);
        list_box.add_css_class("file-list-view");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_top(6);
        list_box.set_margin_bottom(6);

        let list_scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();
        list_scroll.set_child(Some(&list_box));

        let file_view_stack = gtk4::Stack::new();
        file_view_stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        file_view_stack.add_named(&grid_overlay, Some("grid"));
        file_view_stack.add_named(&list_scroll, Some("list"));
        file_view_stack.set_visible_child_name("grid");

        let status = adw::StatusPage::builder()
            .title("No device selected")
            .description("Select a device from the sidebar to browse its files.")
            .icon_name("phone-symbolic")
            .vexpand(true)
            .build();

        let main_stack = gtk4::Stack::new();
        main_stack.set_vexpand(true);
        main_stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        main_stack.add_named(&file_view_stack, Some("files"));
        main_stack.add_named(&status, Some("empty"));
        main_stack.set_visible_child_name("empty");
        root.append(&main_stack);

        let current_path = Rc::new(RefCell::new(PathBuf::from("/")));
        let device = Rc::new(RefCell::new(None));
        let local_mode = Rc::new(RefCell::new(false));
        let fuse_mount: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let show_hidden = Rc::new(RefCell::new(false));
        let zoom = Rc::new(Cell::new(64));
        let entries = Rc::new(RefCell::new(Vec::new()));
        let history_back = Rc::new(RefCell::new(Vec::new()));
        let history_forward = Rc::new(RefCell::new(Vec::new()));
        let search_query = Rc::new(RefCell::new(String::new()));
        let on_event: Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>> = Rc::new(RefCell::new(None));
        let view_mode = Rc::new(RefCell::new(ViewMode::Grid));

        let browser = Self {
            root,
            list_box,
            grid_box,
            file_view_stack,
            grid_overlay,
            main_stack,
            view_mode,
            back_button,
            forward_button,
            up_button,
            refresh_button,
            upload_button,
            download_button,
            new_folder_button,
            open_external_button,
            breadcrumb_container,
            path_entry,
            path_stack,
            path_edit_toggle,
            search_bar,
            search_entry,
            search_button,
            banner_box,
            banner_status_label,
            banner_transport_label,
            banner_battery_label,
            banner_progress,
            banner_pause_btn,
            banner_cancel_btn,
            sub_header_count_label,
            sub_header_path_label,
            current_path,
            device,
            local_mode,
            fuse_mount,
            show_hidden,
            zoom,
            entries,
            history_back,
            history_forward,
            search_query,
            on_event,
        };

        browser.wire_controls();
        browser.set_buttons_sensitive(false);
        browser
    }

    pub fn on_event<F: Fn(BrowserEvent) + 'static>(&self, f: F) {
        *self.on_event.borrow_mut() = Some(Box::new(f));
    }

    /// Fire a browser event programmatically (header menus use this).
    pub fn emit(&self, ev: BrowserEvent) {
        if let Some(cb) = self.on_event.borrow().as_ref() {
            cb(ev);
        }
    }

    pub fn view_mode(&self) -> ViewMode { *self.view_mode.borrow() }

    pub fn set_view_mode(&self, mode: ViewMode) {
        *self.view_mode.borrow_mut() = mode;
        match mode {
            ViewMode::Grid => {
                self.file_view_stack.set_visible_child_name("grid");
            }
            ViewMode::List => {
                self.file_view_stack.set_visible_child_name("list");
            }
        }
    }

    /// Show/hide the "Connected via ADB" banner (only real devices).
    pub fn set_connected(&self, on: bool) {
        self.banner_box.set_visible(on);
    }

    /// Fill the banner + storage metadata from live `adb` data.
    pub fn set_device_info(&self, info: &crate::device_list::DeviceEntry) {
        let transport_text = match info.transport {
            "wifi" => format!("Wi-Fi • {}", info.serial),
            _ => "USB".to_string(),
        };
        self.banner_transport_label.set_label(&transport_text);
        match info.battery_pct {
            Some(pct) => {
                self.banner_battery_label.set_label(&format!("BAT {pct}%"));
                self.banner_battery_label.set_visible(true);
            }
            None => self.banner_battery_label.set_visible(false),
        }
    }

    pub fn update_transfer_banner(&self, active: bool, text: &str, fraction: f64) {
        if active {
            self.banner_status_label.set_label(text);
            self.banner_progress.set_fraction(fraction);
            self.banner_progress.set_visible(true);
            // Only take up banner space while there is something to pause.
            self.banner_pause_btn.set_visible(true);
            self.banner_cancel_btn.set_visible(true);
            self.banner_pause_btn.set_sensitive(true);
            self.banner_cancel_btn.set_sensitive(true);
        } else {
            self.banner_status_label.set_label("Ready");
            self.banner_progress.set_fraction(0.0);
            self.banner_progress.set_visible(false);
            self.banner_pause_btn.set_visible(false);
            self.banner_cancel_btn.set_visible(false);
        }
    }

    pub fn set_device(&self, device: Option<&str>) {
        *self.device.borrow_mut() = device.map(|s| s.to_string());
        *self.local_mode.borrow_mut() = false;
        *self.current_path.borrow_mut() = PathBuf::from("/");
        self.history_back.borrow_mut().clear();
        self.history_forward.borrow_mut().clear();
        self.update_nav_buttons();

        if let Some(dev) = device {
            self.set_buttons_sensitive(true);
            self.set_connected(true);
            self.show_list();
            self.sub_header_path_label.set_label(&format!("device:{dev}:/"));
            self.render_breadcrumbs(&PathBuf::from("/"));
        } else {
            self.set_buttons_sensitive(false);
            self.set_connected(false);
            self.clear();
            self.show_empty();
            self.sub_header_path_label.set_label("device:none:/");
            self.render_breadcrumbs(&PathBuf::from("/"));
        }
    }

    pub fn current_path(&self) -> PathBuf { self.current_path.borrow().clone() }
    pub fn device(&self) -> Option<String> { self.device.borrow().clone() }

    /// Switch to browsing the local Linux filesystem, starting at `/`.
    pub fn set_local_mode(&self) {
        *self.local_mode.borrow_mut() = true;
        *self.device.borrow_mut() = None;
        *self.fuse_mount.borrow_mut() = None;
        *self.current_path.borrow_mut() = PathBuf::from("/");
        self.history_back.borrow_mut().clear();
        self.history_forward.borrow_mut().clear();
        self.set_buttons_sensitive(true);
        self.set_connected(false);
        self.show_list();
        self.update_nav_buttons();
    }

    pub fn is_local_mode(&self) -> bool { *self.local_mode.borrow() }

    /// Navigate back through history (button + Alt+Left).
    pub fn go_back(&self) {
        if let Some(prev) = self.history_back.borrow_mut().pop() {
            self.history_forward.borrow_mut().push(self.current_path.borrow().clone());
            if let Some(cb) = self.on_event.borrow().as_ref() {
                cb(BrowserEvent::Navigate(prev));
            }
        }
        self.update_nav_buttons();
    }

    /// Navigate forward through history (button + Alt+Right).
    pub fn go_forward(&self) {
        if let Some(next) = self.history_forward.borrow_mut().pop() {
            self.history_back.borrow_mut().push(self.current_path.borrow().clone());
            if let Some(cb) = self.on_event.borrow().as_ref() {
                cb(BrowserEvent::Navigate(next));
            }
        }
        self.update_nav_buttons();
    }

    /// Zoom the grid icons one step in (Ctrl+=) / out (Ctrl+-).
    pub fn zoom_in(&self) {
        let cur = self.zoom.get();
        if cur < 128 {
            self.zoom.set((cur * 2).min(128));
            let all = self.entries.borrow().clone();
            self.set_entries(all);
        }
    }

    pub fn zoom_out(&self) {
        let cur = self.zoom.get();
        if cur > 48 {
            self.zoom.set((cur / 2).max(48));
            let all = self.entries.borrow().clone();
            self.set_entries(all);
        }
    }

    /// Toggle dotfile visibility and rebuild the view.
    pub fn toggle_show_hidden(&self) {
        *self.show_hidden.borrow_mut() = !*self.show_hidden.borrow();
        let all = self.entries.borrow().clone();
        self.set_entries(all);
    }

    pub fn show_hidden(&self) -> bool { *self.show_hidden.borrow() }

    /// Select every entry in the active view (Ctrl+A / kebab menu).
    pub fn select_all_active(&self) {
        if self.file_view_stack.visible_child_name().map(|n| n == "grid").unwrap_or(true) {
            self.grid_box.select_all();
        } else {
            self.list_box.select_all();
        }
    }

    /// All currently selected entries in whichever view is active.
    pub fn selected_entries(&self) -> Vec<DirEntry> {
        let is_grid = self
            .file_view_stack
            .visible_child_name()
            .map(|n| n == "grid")
            .unwrap_or(true);
        if is_grid {
            selected_entries_from_grid(&self.grid_box, &self.entries)
        } else {
            selected_entries_from_list(&self.list_box, &self.entries)
        }
    }

    /// Set the device's FUSE mountpoint (drag & drop out of the app).
    pub fn set_fuse_mount(&self, mountpoint: Option<&str>) {
        *self.fuse_mount.borrow_mut() = mountpoint.map(|s| s.trim_end_matches('/').to_string());
    }

    pub fn fuse_mount(&self) -> Option<String> { self.fuse_mount.borrow().clone() }

    /// The filesystem path an entry maps to for drag & drop: the local path
    /// in local mode, the FUSE-mounted path in device mode (None if unmounted).
    fn dnd_fs_path(&self, name: &str) -> Option<PathBuf> {
        let mut p = self.current_path.borrow().clone();
        p.push(name);
        if self.is_local_mode() {
            Some(p)
        } else {
            self.fuse_mount.borrow().as_ref().map(|mp| PathBuf::from(format!("{}{}", mp, p.display())))
        }
    }

    /// Drag source that offers the entry as a gio File plus its path text.
    fn add_drag_source(&self, widget: &impl IsA<gtk4::Widget>, name: &str) {
        let drag = gtk4::DragSource::new();
        drag.set_actions(gdk4::DragAction::COPY | gdk4::DragAction::MOVE);
        let fs_path = self.dnd_fs_path(name);
        drag.connect_prepare(move |_src, _x, _y| {
            let Some(fs_path) = fs_path.clone() else { return None };
            let file = gtk4::gio::File::for_path(&fs_path);
            let file_prov = gdk4::ContentProvider::for_value(&file.to_value());
            let text_prov = gdk4::ContentProvider::for_value(&fs_path.to_string_lossy().to_string().to_value());
            Some(gdk4::ContentProvider::new_union(&[file_prov, text_prov]))
        });
        widget.add_controller(drag);
    }

    /// Drop target for dropping files onto a directory entry.
    fn add_dir_drop_target(&self, widget: &impl IsA<gtk4::Widget>, dir_name: &str) {
        let drop = gtk4::DropTarget::new(
            gtk4::gio::File::static_type(),
            gdk4::DragAction::COPY | gdk4::DragAction::MOVE,
        );
        let on_ev = self.on_event.clone();
        let curr = self.current_path.clone();
        let dir_name = dir_name.to_string();
        drop.connect_drop(move |_target, value, _x, _y| {
            let dropped: Option<PathBuf> = value.get::<gtk4::gio::File>().ok().and_then(|f| f.path());
            let Some(src) = dropped else { return false };
            let mut target_dir = curr.borrow().clone();
            target_dir.push(&dir_name);
            let from_dir = curr.borrow().clone();
            if let Some(cb) = on_ev.borrow().as_ref() {
                cb(BrowserEvent::DropFiles { from_dir, target_dir, files: vec![src] });
            }
            true
        });
        widget.add_controller(drop);
    }

    pub fn set_entries(&self, entries: Vec<DirEntry>) {
        self.clear();
        *self.entries.borrow_mut() = entries.clone();

        let count = entries.len();

        self.sub_header_count_label.set_label(&format!("Files & Folders ({} items)", count));

        for e in entries {
            // Hide dotfiles unless toggled on (entries vec keeps everything).
            if !*self.show_hidden.borrow() && e.name.starts_with('.') {
                continue;
            }
            // 1. Populate List View
            let row = adw::ActionRow::builder()
                .title(&e.name)
                .subtitle(if e.is_dir {
                    "Folder".to_string()
                } else {
                    let d = e.display_date();
                    if d.is_empty() { e.display_size() } else { format!("{} • {}", e.display_size(), d) }
                })
                .activatable(true)
                .build();
            row.add_css_class("file-row");
            row.set_widget_name(&e.name);

            let icon = match asset_icon_for(&e)
                .and_then(|name| asset_icon_dir().map(|dir| dir.join(name)))
                .map(|p| gtk4::Image::from_file(&p))
            {
                Some(img) => img,
                None => gtk4::Image::from_icon_name(e.icon_name()),
            };
            icon.set_pixel_size(22);
            if !asset_icon_for(&e).is_some() {
                if e.is_dir {
                    icon.add_css_class("folder-icon");
                } else if e.name.ends_with(".apk") {
                    icon.add_css_class("apk-icon");
                } else {
                    icon.add_css_class("file-icon");
                }
            }
            row.add_prefix(&icon);

            if e.is_dir {
                let chevron = gtk4::Image::from_icon_name("go-next-symbolic");
                chevron.add_css_class("dim-label");
                row.add_suffix(&chevron);
            }

            // Right-click: select-under-cursor, then selection-aware menu.
            let right_click = gtk4::GestureClick::new();
            right_click.set_button(3);
            {
                let entry_rc = e.clone();
                let list_box_rc = self.list_box.clone();
                let entries_rc = self.entries.clone();
                let row_rc = row.clone();
                let parent_row = row.clone();
                let on_ev_rc = self.on_event.clone();
                let curr_path_rc = self.current_path.clone();
                let dev_rc = self.device.clone();
                let self_local = self.local_mode.clone();
                right_click.connect_pressed(move |_gesture, _n, x, y| {
                    let in_selection = list_box_rc.selected_rows().iter().any(|r| r == &row_rc);
                    if !in_selection {
                        list_box_rc.select_row(Some(&row_rc));
                    }
                    let selected = selected_entries_from_list(&list_box_rc, &entries_rc);
                    show_context_menu(
                        &parent_row,
                        x,
                        y,
                        &selected,
                        &entry_rc,
                        &on_ev_rc,
                        &curr_path_rc.borrow(),
                        dev_rc.borrow().as_deref().unwrap_or(""),
                        *self_local.borrow(),
                    );
                });
            }
            row.add_controller(right_click);

            // Drag out of the app; drop onto folder rows to move/copy in.
            self.add_drag_source(&row, &e.name);
            if e.is_dir {
                self.add_dir_drop_target(&row, &e.name);
            }
            self.list_box.append(&row);

            // 2. Populate Grid View Card
            // The FlowBox stretches each FlowBoxChild to share the row
            // width, so the card must FILL its cell (not center a fixed
            // 78px box inside it) — otherwise the leftover becomes the
            // huge dead gutters seen in screenshots.
            let card = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            card.add_css_class("grid-item-card");
            card.set_valign(gtk4::Align::Start);
            let icon_px = self.zoom.get();
            // Minimum width only: the card expands with its cell, and the
            // height stays natural so labels never clip.
            card.set_size_request(108, -1);
            card.set_halign(gtk4::Align::Fill);
            card.set_hexpand(true);
            card.set_valign(gtk4::Align::Start);
            card.set_widget_name(&e.name);

            // Design uses full-color icons: bundled SVGs first, theme fallback.
            let icon_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            icon_box.set_size_request(icon_px as i32, icon_px as i32);
            icon_box.set_halign(gtk4::Align::Center);
            icon_box.set_valign(gtk4::Align::Center);
            let grid_icon = match asset_icon_for(&e)
                .and_then(|name| asset_icon_dir().map(|dir| dir.join(name)))
                .map(|p| gtk4::Image::from_file(&p))
            {
                Some(img) => img,
                None => {
                    let img = gtk4::Image::from_icon_name(e.icon_name());
                    if e.is_dir {
                        img.add_css_class("folder-icon");
                    } else if e.name.ends_with(".apk") {
                        img.add_css_class("apk-icon");
                    } else {
                        img.add_css_class("file-icon");
                    }
                    img
                }
            };
            grid_icon.set_pixel_size(icon_px as i32);
            icon_box.append(&grid_icon);
            card.append(&icon_box);

            let grid_title = gtk4::Label::builder()
                .label(&e.name)
                .justify(gtk4::Justification::Center)
                .xalign(0.5)
                .wrap(true)
                .wrap_mode(gtk4::pango::WrapMode::WordChar)
                .lines(2)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .max_width_chars(14)
                .hexpand(true)
                .build();
            // Fill the (now fluid-width) card; wrapping handles long names
            // so the label can never inflate the column.
            grid_title.set_halign(gtk4::Align::Fill);
            grid_title.add_css_class("grid-item-title");
            card.append(&grid_title);

            let sub_text = if e.is_dir { "Folder".to_string() } else { e.display_size() };
            let grid_sub = gtk4::Label::builder()
                .label(sub_text)
                .justify(gtk4::Justification::Center)
                .xalign(0.5)
                .hexpand(true)
                .build();
            grid_sub.set_halign(gtk4::Align::Fill);
            grid_sub.add_css_class("grid-item-sub");
            card.append(&grid_sub);

            // Right-click: select-under-cursor, then selection-aware menu.
            let card_rc = gtk4::GestureClick::new();
            card_rc.set_button(3);
            {
                let entry_card_rc = e.clone();
                let grid_rc = self.grid_box.clone();
                let entries_rc = self.entries.clone();
                let card_ref = card.clone();
                let parent_card = card.clone();
                let on_ev_card_rc = self.on_event.clone();
                let curr_p_card = self.current_path.clone();
                let dev_card = self.device.clone();
                let self_local = self.local_mode.clone();
                card_rc.connect_pressed(move |_gesture, _n, x, y| {
                    // The card is wrapped in a FlowBoxChild; compare/select that.
                    let child = card_ref
                        .parent()
                        .and_then(|p| p.downcast::<gtk4::FlowBoxChild>().ok());
                    let in_selection = match &child {
                        Some(c) => grid_rc.selected_children().iter().any(|sel| sel == c),
                        None => false,
                    };
                    if !in_selection {
                        if let Some(c) = &child {
                            grid_rc.select_child(c);
                        }
                    }
                    let selected = selected_entries_from_grid(&grid_rc, &entries_rc);
                    show_context_menu(
                        &parent_card,
                        x,
                        y,
                        &selected,
                        &entry_card_rc,
                        &on_ev_card_rc,
                        &curr_p_card.borrow(),
                        dev_card.borrow().as_deref().unwrap_or(""),
                        *self_local.borrow(),
                    );
                });
            }
            card.add_controller(card_rc);

            // Drag out of the app; drop onto folder cards to move/copy in.
            self.add_drag_source(&card, &e.name);
            if e.is_dir {
                self.add_dir_drop_target(&card, &e.name);
            }
            self.grid_box.append(&card);
        }
    }

    pub fn show_path(&self, path: PathBuf) {
        let old_path = self.current_path.borrow().clone();
        if old_path != path && old_path != PathBuf::from("") {
            self.history_back.borrow_mut().push(old_path);
        }
        *self.current_path.borrow_mut() = path.clone();
        self.render_breadcrumbs(&path);
        self.path_entry.set_text(&path.to_string_lossy());

        let dev_str = self.device.borrow().clone().unwrap_or_else(|| "device".to_string());
        if self.is_local_mode() {
            self.sub_header_path_label.set_label(&format!("local:{}", path.display()));
        } else {
            self.sub_header_path_label.set_label(&format!("device:{}:{}", dev_str, path.display()));
        }
        self.update_nav_buttons();
    }

    pub fn set_loading(&self, loading: bool) {
        self.refresh_button.set_sensitive(!loading);
    }

    pub fn is_loading(&self) -> bool {
        !self.refresh_button.is_sensitive()
    }

    pub fn show_empty(&self) {
        self.main_stack.set_visible_child_name("empty");
    }

    pub fn show_list(&self) {
        self.main_stack.set_visible_child_name("files");
    }

    fn clear(&self) {
        *self.entries.borrow_mut() = Vec::new();
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }
        while let Some(child) = self.grid_box.first_child() {
            self.grid_box.remove(&child);
        }
    }

    fn set_buttons_sensitive(&self, on: bool) {
        self.up_button.set_sensitive(on);
        self.refresh_button.set_sensitive(on);
        self.upload_button.set_sensitive(on);
        self.new_folder_button.set_sensitive(on);
        self.open_external_button.set_sensitive(on);
        self.search_button.set_sensitive(on);
        self.download_button.set_sensitive(false);
    }

    fn update_nav_buttons(&self) {
        let has_back = !self.history_back.borrow().is_empty();
        let has_fwd = !self.history_forward.borrow().is_empty();
        let is_root = *self.current_path.borrow() == PathBuf::from("/");
        self.back_button.set_sensitive(has_back);
        self.forward_button.set_sensitive(has_fwd);
        self.up_button.set_sensitive(!is_root);
    }

    fn render_breadcrumbs(&self, path: &PathBuf) {
        while let Some(child) = self.breadcrumb_container.first_child() {
            self.breadcrumb_container.remove(&child);
        }

        let path_str = path.to_string_lossy();

        if self.is_local_mode() {
            // Local Linux filesystem: root pill is "Linux Root".
            let root_btn = gtk4::Button::builder()
                .icon_name("drive-harddisk-symbolic")
                .label("Linux Root")
                .tooltip_text("Linux filesystem root")
                .build();
            root_btn.add_css_class("nav-pill-btn");
            if path_str == "/" {
                root_btn.add_css_class("current");
            }
            let on_ev_root = self.on_event.clone();
            root_btn.connect_clicked(move |_| {
                if let Some(cb) = on_ev_root.borrow().as_ref() {
                    cb(BrowserEvent::Navigate(PathBuf::from("/")));
                }
            });
            self.breadcrumb_container.append(&root_btn);
            if path_str == "/" {
                return;
            }
            let comps: Vec<_> = path.iter().filter(|c| *c != "/").collect();
            let mut accum = PathBuf::from("/");
            for (i, comp) in comps.iter().enumerate() {
                let sep = gtk4::Label::new(Some("/"));
                sep.add_css_class("nav-pill-sep");
                self.breadcrumb_container.append(&sep);

                accum.push(comp);
                let target_path = accum.clone();
                let is_last = i == comps.len() - 1;

                let label_text = if is_last {
                    format!("{} ▾", comp.to_string_lossy())
                } else {
                    comp.to_string_lossy().to_string()
                };

                let seg_btn = gtk4::Button::with_label(&label_text);
                seg_btn.add_css_class("nav-pill-btn");
                if is_last {
                    seg_btn.add_css_class("current");
                }
                let on_ev_seg = self.on_event.clone();
                seg_btn.connect_clicked(move |_| {
                    if let Some(cb) = on_ev_seg.borrow().as_ref() {
                        cb(BrowserEvent::Navigate(target_path.clone()));
                    }
                });
                self.breadcrumb_container.append(&seg_btn);
            }
            return;
        }

        let dev_name = self.device.borrow().clone().unwrap_or_else(|| "No device".to_string());

        // 1. Device pill button
        let dev_btn = gtk4::Button::builder()
            .icon_name("phone-symbolic")
            .label(&dev_name)
            .tooltip_text("Device Storage")
            .build();
        dev_btn.add_css_class("nav-pill-btn");
        let on_event = self.on_event.clone();
        dev_btn.connect_clicked(move |_| {
            if let Some(cb) = on_event.borrow().as_ref() {
                cb(BrowserEvent::Navigate(PathBuf::from("/sdcard")));
            }
        });
        self.breadcrumb_container.append(&dev_btn);

        let path_str = path.to_string_lossy();
        if path_str == "/" {
            return;
        }

        // Separator /
        let sep0 = gtk4::Label::new(Some("/"));
        sep0.add_css_class("nav-pill-sep");
        self.breadcrumb_container.append(&sep0);

        if path_str.starts_with("/sdcard") {
            let is_storage_root = path_str == "/sdcard" || path_str == "/sdcard/";
            let storage_btn = gtk4::Button::with_label(if is_storage_root { "Internal Storage ▾" } else { "Internal Storage" });
            storage_btn.add_css_class("nav-pill-btn");
            if is_storage_root {
                storage_btn.add_css_class("current");
            }
            let on_ev_storage = self.on_event.clone();
            storage_btn.connect_clicked(move |_| {
                if let Some(cb) = on_ev_storage.borrow().as_ref() {
                    cb(BrowserEvent::Navigate(PathBuf::from("/sdcard")));
                }
            });
            self.breadcrumb_container.append(&storage_btn);

            let rest = path.strip_prefix("/sdcard").unwrap_or(path);
            let comps: Vec<_> = rest.iter().filter(|c| *c != "").collect();
            let mut accum = PathBuf::from("/sdcard");
            for (i, comp) in comps.iter().enumerate() {
                let sep = gtk4::Label::new(Some("/"));
                sep.add_css_class("nav-pill-sep");
                self.breadcrumb_container.append(&sep);

                accum.push(comp);
                let target_path = accum.clone();
                let is_last = i == comps.len() - 1;

                let label_text = if is_last {
                    format!("{} ▾", comp.to_string_lossy())
                } else {
                    comp.to_string_lossy().to_string()
                };

                let seg_btn = gtk4::Button::with_label(&label_text);
                seg_btn.add_css_class("nav-pill-btn");
                if is_last {
                    seg_btn.add_css_class("current");
                }
                let on_ev_seg = self.on_event.clone();
                seg_btn.connect_clicked(move |_| {
                    if let Some(cb) = on_ev_seg.borrow().as_ref() {
                        cb(BrowserEvent::Navigate(target_path.clone()));
                    }
                });
                self.breadcrumb_container.append(&seg_btn);
            }
        } else {
            let comps: Vec<_> = path.iter().filter(|c| *c != "/").collect();
            let mut accum = PathBuf::from("/");
            for (i, comp) in comps.iter().enumerate() {
                let sep = gtk4::Label::new(Some("/"));
                sep.add_css_class("nav-pill-sep");
                self.breadcrumb_container.append(&sep);

                accum.push(comp);
                let target_path = accum.clone();
                let is_last = i == comps.len() - 1;

                let label_text = if is_last {
                    format!("{} ▾", comp.to_string_lossy())
                } else {
                    comp.to_string_lossy().to_string()
                };

                let seg_btn = gtk4::Button::with_label(&label_text);
                seg_btn.add_css_class("nav-pill-btn");
                if is_last {
                    seg_btn.add_css_class("current");
                }
                let on_ev_seg = self.on_event.clone();
                seg_btn.connect_clicked(move |_| {
                    if let Some(cb) = on_ev_seg.borrow().as_ref() {
                        cb(BrowserEvent::Navigate(target_path.clone()));
                    }
                });
                self.breadcrumb_container.append(&seg_btn);
            }
        }
    }

    fn wire_controls(&self) {
        // Up
        let on_ev_up = self.on_event.clone();
        self.up_button.connect_clicked(move |_| {
            if let Some(cb) = on_ev_up.borrow().as_ref() {
                cb(BrowserEvent::Up);
            }
        });

        // Back / Forward share the history logic with keyboard shortcuts.
        {
            let browser = self.clone();
            self.back_button.connect_clicked(move |_| browser.go_back());
        }
        {
            let browser = self.clone();
            self.forward_button.connect_clicked(move |_| browser.go_forward());
        }

        // Refresh
        let on_ev_ref = self.on_event.clone();
        self.refresh_button.connect_clicked(move |_| {
            if let Some(cb) = on_ev_ref.borrow().as_ref() {
                cb(BrowserEvent::Refresh);
            }
        });

        // Upload
        let on_ev_upld = self.on_event.clone();
        self.upload_button.connect_clicked(move |_| {
            if let Some(cb) = on_ev_upld.borrow().as_ref() {
                cb(BrowserEvent::Upload);
            }
        });

        // Download: every selected file (multi-select aware).
        let on_ev_dnld = self.on_event.clone();
        let browser_dl = self.clone();
        self.download_button.connect_clicked(move |_| {
            let files: Vec<DirEntry> = browser_dl
                .selected_entries()
                .into_iter()
                .filter(|e| !e.is_dir)
                .collect();
            if !files.is_empty() {
                if let Some(cb) = on_ev_dnld.borrow().as_ref() {
                    cb(BrowserEvent::Download(files));
                }
            }
        });

        // New Folder
        let on_ev_new = self.on_event.clone();
        let root_for_dialog = self.root.clone();
        self.new_folder_button.connect_clicked(move |_| {
            show_new_folder_dialog(&root_for_dialog, &on_ev_new);
        });

        // Open External (Nautilus / System File Manager)
        let on_ev_ext = self.on_event.clone();
        let curr_p_ext = self.current_path.clone();
        self.open_external_button.connect_clicked(move |_| {
            let path = curr_p_ext.borrow().clone();
            if let Some(cb) = on_ev_ext.borrow().as_ref() {
                cb(BrowserEvent::OpenExternal(path));
            }
        });

        // Banner Pause & Cancel
        let on_ev_pause = self.on_event.clone();
        self.banner_pause_btn.connect_clicked(move |_| {
            if let Some(cb) = on_ev_pause.borrow().as_ref() {
                cb(BrowserEvent::PauseTransfer);
            }
        });

        let on_ev_cancel = self.on_event.clone();
        self.banner_cancel_btn.connect_clicked(move |_| {
            if let Some(cb) = on_ev_cancel.borrow().as_ref() {
                cb(BrowserEvent::CancelTransfer);
            }
        });

        // Path edit toggle
        let path_stack = self.path_stack.clone();
        let path_entry = self.path_entry.clone();
        let curr_p_entry = self.current_path.clone();
        self.path_edit_toggle.connect_toggled(move |btn| {
            if btn.is_active() {
                path_entry.set_text(&curr_p_entry.borrow().to_string_lossy());
                path_stack.set_visible_child_name("entry");
                path_entry.grab_focus();
            } else {
                path_stack.set_visible_child_name("breadcrumbs");
            }
        });

        // Direct path entry activate (pressing Enter)
        let on_ev_entry = self.on_event.clone();
        let edit_toggle_entry = self.path_edit_toggle.clone();
        self.path_entry.connect_activate(move |entry| {
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                let mut path = PathBuf::from(text);
                if !path.is_absolute() {
                    path = PathBuf::from("/").join(path);
                }
                edit_toggle_entry.set_active(false);
                if let Some(cb) = on_ev_entry.borrow().as_ref() {
                    cb(BrowserEvent::Navigate(path));
                }
            }
        });

        // Search bar toggle
        let search_bar = self.search_bar.clone();
        self.search_button.connect_toggled(move |btn| {
            search_bar.set_search_mode(btn.is_active());
        });

        // Search filtering (applies to both list_box and grid_box)
        let search_query = self.search_query.clone();
        let lb_filter = self.list_box.clone();
        let gb_filter = self.grid_box.clone();
        self.search_entry.connect_search_changed(move |entry| {
            *search_query.borrow_mut() = entry.text().to_string();
            lb_filter.invalidate_filter();
            gb_filter.invalidate_filter();
        });

        let search_query_filter = self.search_query.clone();
        self.list_box.set_filter_func(move |row| {
            let q = search_query_filter.borrow();
            let query = q.trim().to_lowercase();
            if query.is_empty() { return true; }
            if let Some(ar) = row.downcast_ref::<adw::ActionRow>() {
                ar.title().to_lowercase().contains(&query)
            } else {
                true
            }
        });

        let search_query_grid = self.search_query.clone();
        self.grid_box.set_filter_func(move |child| {
            let q = search_query_grid.borrow();
            let query = q.trim().to_lowercase();
            if query.is_empty() { return true; }
            if let Some(inner) = child.child() {
                inner.widget_name().to_lowercase().contains(&query)
            } else {
                true
            }
        });

        // List selection (multi): enable download when any file is selected.
        {
            let lb_sel = self.list_box.clone();
            let entries_sel = self.entries.clone();
            let dl_btn = self.download_button.clone();
            let on_event_sel = self.on_event.clone();
            self.list_box.connect_selected_rows_changed(move |_lb| {
                let files = selected_entries_from_list(&lb_sel, &entries_sel);
                dl_btn.set_sensitive(files.iter().any(|e| !e.is_dir));
                if let Some(cb) = on_event_sel.borrow().as_ref() {
                    cb(BrowserEvent::Selected(files.first().cloned()));
                }
            });
        }

        // List Row activated (Enter key / double-click fallback)
        let entries_act = self.entries.clone();
        let on_event_act = self.on_event.clone();
        let local_act = self.local_mode.clone();
        let curr_act = self.current_path.clone();
        self.list_box.connect_row_activated(move |_lb, row| {
            let idx = row.index() as usize;
            let entries = entries_act.borrow();
            if let Some(e) = entries.get(idx) {
                if e.is_dir || e.is_symlink {
                    if let Some(cb) = on_event_act.borrow().as_ref() {
                        cb(BrowserEvent::OpenDir(e.clone()));
                    }
                } else if *local_act.borrow() {
                    // Local mode: open the file with its default application.
                    let mut p = curr_act.borrow().clone();
                    p.push(&e.name);
                    if let Some(cb) = on_event_act.borrow().as_ref() {
                        cb(BrowserEvent::OpenExternal(p));
                    }
                }
            }
        });

        // Grid selection (multi): enable download when any file is selected.
        {
            let grid_sel = self.grid_box.clone();
            let entries_gsel = self.entries.clone();
            let dl_btn_g = self.download_button.clone();
            self.grid_box.connect_selected_children_changed(move |_fb| {
                let files = selected_entries_from_grid(&grid_sel, &entries_gsel);
                dl_btn_g.set_sensitive(files.iter().any(|e| !e.is_dir));
            });
        }

        // Grid Selection / Activation handling
        let entries_sel_grid = self.entries.clone();
        let on_ev_sel_grid = self.on_event.clone();
        let dl_btn_grid = self.download_button.clone();
        let local_grid = self.local_mode.clone();
        let curr_grid = self.current_path.clone();
        self.grid_box.connect_child_activated(move |_fb, child| {
            let idx = child.index() as usize;
            if let Some(entry) = entries_sel_grid.borrow().get(idx).cloned() {
                if entry.is_dir || entry.is_symlink {
                    if let Some(cb) = on_ev_sel_grid.borrow().as_ref() {
                        cb(BrowserEvent::OpenDir(entry));
                    }
                } else {
                    dl_btn_grid.set_sensitive(true);
                    if *local_grid.borrow() {
                        let mut p = curr_grid.borrow().clone();
                        p.push(&entry.name);
                        if let Some(cb) = on_ev_sel_grid.borrow().as_ref() {
                            cb(BrowserEvent::OpenExternal(p));
                        }
                    } else if let Some(cb) = on_ev_sel_grid.borrow().as_ref() {
                        cb(BrowserEvent::Selected(Some(entry)));
                    }
                }
            }
        });

        let drag = gtk4::GestureDrag::new();
        drag.set_button(1);
        drag.set_exclusive(true);
        drag.set_propagation_phase(gtk4::PropagationPhase::Capture);

        let grid_begin = self.grid_box.clone();
        let grid_update = self.grid_box.clone();
        let overlay_begin = self.grid_overlay.clone();
        let overlay_update = self.grid_overlay.clone();
        let overlay_end = self.grid_overlay.clone();
        let start_grid: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));
        let start_grid_update = start_grid.clone();
        let start_grid_end = start_grid.clone();
        let start_ov: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));
        let start_ov_update = start_ov.clone();
        let start_ov_end = start_ov.clone();
        let band: Rc<RefCell<Option<gtk4::Box>>> = Rc::new(RefCell::new(None));
        let band_update = band.clone();
        let band_end = band.clone();
        let additive: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        let additive_update = additive.clone();

        drag.connect_drag_begin(move |d, x, y| {
            // Press must be on empty space (not a card).
            let mut w = grid_begin.pick(x, y, gtk4::PickFlags::DEFAULT);
            let mut hit_child = false;
            while let Some(widget) = w {
                if widget.is::<gtk4::FlowBoxChild>() {
                    hit_child = true;
                    break;
                }
                w = widget.parent();
            }
            if hit_child {
                d.set_state(gtk4::EventSequenceState::Denied);
                return;
            }
            d.set_state(gtk4::EventSequenceState::Claimed);

            let shift = d
                .current_event_state()
                .contains(gdk4::ModifierType::SHIFT_MASK);
            *additive.borrow_mut() = shift;
            if !shift {
                grid_begin.unselect_all();
            }

            let Some(p) = grid_begin.translate_coordinates(&overlay_begin, x, y) else {
                d.set_state(gtk4::EventSequenceState::Denied);
                return;
            };
            // Defensive: a previous gesture that died without drag-end
            // (grab break, cancel) must not leave a stale band behind.
            if let Some(stale) = band.borrow_mut().take() {
                overlay_begin.remove_overlay(&stale);
            }
            *start_grid.borrow_mut() = Some((x, y));
            *start_ov.borrow_mut() = Some(p);

            let band_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            band_box.add_css_class("rubber-band");
            // Overlay children default to Fill alignment, which stretches
            // the band across the whole viewport — pin it top-left so the
            // margins + size request below actually place the rectangle.
            band_box.set_halign(gtk4::Align::Start);
            band_box.set_valign(gtk4::Align::Start);
            overlay_begin.add_overlay(&band_box);
            *band.borrow_mut() = Some(band_box);
        });

        drag.connect_drag_update(move |d, ox, oy| {
            let (Some((gx, gy)), Some((sx, sy))) =
                (start_grid_update.borrow().as_ref().copied(), start_ov_update.borrow().as_ref().copied())
            else {
                return;
            };
            let Some(band_box) = band_update.borrow().as_ref().cloned() else { return };

            let cur = grid_update.translate_coordinates(&overlay_update, gx + ox, gy + oy);
            let Some((cx, cy)) = cur else { return };

            let rx = sx.min(cx);
            let ry = sy.min(cy);
            let rw = (sx - cx).abs();
            let rh = (sy - cy).abs();
            band_box.set_margin_start(rx as i32);
            band_box.set_margin_top(ry as i32);
            band_box.set_size_request(rw as i32, rh as i32);

            // Select every child intersecting the rectangle.
            let rect = (rx, ry, rx + rw, ry + rh);
            let additive_now = *additive_update.borrow();
            let mut child = grid_update.first_child();
            while let Some(fc) = child {
                child = fc.next_sibling();
                let Ok(fcc) = fc.downcast::<gtk4::FlowBoxChild>() else { continue };
                let Some((ax, ay)) = fcc.translate_coordinates(&overlay_update, 0.0, 0.0) else { continue };
                let alloc = fcc.allocation();
                let (ax2, ay2) = (ax + alloc.width() as f64, ay + alloc.height() as f64);
                let intersects = ax < rect.2 && ax2 > rect.0 && ay < rect.3 && ay2 > rect.1;
                if intersects {
                    grid_update.select_child(&fcc);
                } else if !additive_now {
                    grid_update.unselect_child(&fcc);
                }
            }
        });

        drag.connect_drag_end(move |d, _ox, _oy| {
            d.set_state(gtk4::EventSequenceState::None);
            if let Some(band_box) = band_end.borrow_mut().take() {
                overlay_end.remove_overlay(&band_box);
            }
            *start_grid_end.borrow_mut() = None;
            *start_ov_end.borrow_mut() = None;
        });
        self.grid_box.add_controller(drag);

        {
            let list = self.list_box.clone();
            let click = gtk4::GestureClick::new();
            click.set_button(1);
            click.set_propagation_phase(gtk4::PropagationPhase::Capture);
            click.connect_pressed(move |g, _n, x, y| {
                let mut hit_row = false;
                let mut w = list.pick(x, y, gtk4::PickFlags::DEFAULT);
                while let Some(widget) = w {
                    if widget.is::<gtk4::ListBoxRow>() {
                        hit_row = true;
                        break;
                    }
                    w = widget.parent();
                }
                if !hit_row {
                    list.unselect_all();
                }
                g.set_state(gtk4::EventSequenceState::None);
            });
            self.list_box.add_controller(click);
        }

        // --- Keyboard shortcuts (file-manager essentials) ---
        let shortcuts = gtk4::ShortcutController::new();
        shortcuts.set_scope(gtk4::ShortcutScope::Global);
        self.root.add_controller(shortcuts.clone());

        let add_shortcut = |trigger: &str, action: gtk4::CallbackAction| {
            if let Some(t) = gtk4::ShortcutTrigger::parse_string(trigger) {
                shortcuts.add_shortcut(gtk4::Shortcut::new(Some(t), Some(action.upcast::<gtk4::ShortcutAction>())));
            }
        };
        let emit_ev = |on_ev: &Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>>, ev: BrowserEvent| {
            if let Some(cb) = on_ev.borrow().as_ref() {
                cb(ev);
            }
        };
        let view_is_grid = |stack: &gtk4::Stack| stack.visible_child_name().map(|n| n == "grid").unwrap_or(true);

        // Ctrl+A / Escape — select all / clear selection
        {
            let grid = self.grid_box.clone();
            let list = self.list_box.clone();
            let stack = self.file_view_stack.clone();
            add_shortcut("<Control>a", gtk4::CallbackAction::new(move |_, _| {
                if view_is_grid(&stack) { grid.select_all(); } else { list.select_all(); }
                glib::Propagation::Proceed
            }));
        }
        {
            let grid = self.grid_box.clone();
            let list = self.list_box.clone();
            let stack = self.file_view_stack.clone();
            add_shortcut("Escape", gtk4::CallbackAction::new(move |_, _| {
                if view_is_grid(&stack) { grid.unselect_all(); } else { list.unselect_all(); }
                glib::Propagation::Proceed
            }));
        }
        // Delete — delete the selection (with confirmation)
        {
            let browser = self.clone();
            add_shortcut("Delete", gtk4::CallbackAction::new(move |_, _| {
                let sel = browser.selected_entries();
                if !sel.is_empty() {
                    let label = if sel.len() == 1 { sel[0].name.clone() } else { format!("{} items", sel.len()) };
                    if let Some(w) = browser.root.root() {
                        show_delete_dialog(&w, &label, sel, &browser.on_event);
                    }
                }
                glib::Propagation::Proceed
            }));
        }
        // Alt+Left / Alt+Right / Alt+Up — history & parent
        {
            let browser = self.clone();
            add_shortcut("<Alt>Left", gtk4::CallbackAction::new(move |_, _| { browser.go_back(); glib::Propagation::Proceed }));
        }
        {
            let browser = self.clone();
            add_shortcut("<Alt>Right", gtk4::CallbackAction::new(move |_, _| { browser.go_forward(); glib::Propagation::Proceed }));
        }
        {
            let on_ev = self.on_event.clone();
            add_shortcut("<Alt>Up", gtk4::CallbackAction::new(move |_, _| { emit_ev(&on_ev, BrowserEvent::Up); glib::Propagation::Proceed }));
        }
        // F5 — refresh
        {
            let on_ev = self.on_event.clone();
            add_shortcut("F5", gtk4::CallbackAction::new(move |_, _| { emit_ev(&on_ev, BrowserEvent::Refresh); glib::Propagation::Proceed }));
        }
        // Ctrl+F / Ctrl+L — search / path entry
        {
            let entry = self.search_entry.clone();
            add_shortcut("<Control>f", gtk4::CallbackAction::new(move |_, _| { entry.grab_focus(); glib::Propagation::Proceed }));
        }
        {
            let btn = self.path_edit_toggle.clone();
            add_shortcut("<Control>l", gtk4::CallbackAction::new(move |_, _| { btn.set_active(true); glib::Propagation::Proceed }));
        }
        // Ctrl+Shift+C — pull selection; Ctrl+U — push
        {
            let browser = self.clone();
            let on_ev = self.on_event.clone();
            add_shortcut("<Control><Shift>c", gtk4::CallbackAction::new(move |_, _| {
                let files: Vec<DirEntry> = browser.selected_entries().into_iter().filter(|e| !e.is_dir).collect();
                if !files.is_empty() { emit_ev(&on_ev, BrowserEvent::Download(files)); }
                glib::Propagation::Proceed
            }));
        }
        {
            let on_ev = self.on_event.clone();
            add_shortcut("<Control>u", gtk4::CallbackAction::new(move |_, _| { emit_ev(&on_ev, BrowserEvent::Upload); glib::Propagation::Proceed }));
        }
        // Ctrl+= / Ctrl+- — grid zoom
        {
            let browser = self.clone();
            add_shortcut("<Control>plus", gtk4::CallbackAction::new(move |_, _| {
                browser.zoom_in();
                glib::Propagation::Proceed
            }));
        }
        {
            let browser = self.clone();
            add_shortcut("<Control>equal", gtk4::CallbackAction::new(move |_, _| {
                browser.zoom_in();
                glib::Propagation::Proceed
            }));
        }
        {
            let browser = self.clone();
            add_shortcut("<Control>minus", gtk4::CallbackAction::new(move |_, _| {
                browser.zoom_out();
                glib::Propagation::Proceed
            }));
        }
        // Alt+T — open current dir in terminal; Alt+Return — properties
        {
            let on_ev = self.on_event.clone();
            let curr = self.current_path.clone();
            add_shortcut("<Alt>t", gtk4::CallbackAction::new(move |_, _| {
                emit_ev(&on_ev, BrowserEvent::OpenTerminal(curr.borrow().clone()));
                glib::Propagation::Proceed
            }));
        }
        {
            let browser = self.clone();
            add_shortcut("<Alt>Return", gtk4::CallbackAction::new(move |_, _| {
                let sel = browser.selected_entries();
                if sel.len() == 1 {
                    let e = sel.into_iter().next().unwrap();
                    let mut full = browser.current_path();
                    full.push(&e.name);
                    if let Some(w) = browser.root.root() {
                        let dev = browser.device.borrow().clone().unwrap_or_default();
                        show_properties_dialog(&w, &e, &full, &dev);
                    }
                }
                glib::Propagation::Proceed
            }));
        }
    }
}

/// Helper function to create stylish context menu items
fn create_menu_button(
    icon_name: &str,
    title: &str,
    shortcut: Option<&str>,
    is_highlight: bool,
    is_destructive: bool,
) -> (gtk4::Button, gtk4::Box) {
    let btn = gtk4::Button::new();
    btn.set_has_frame(false);
    if is_destructive {
        btn.add_css_class("destructive");
    }

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    hbox.set_margin_start(6);
    hbox.set_margin_end(6);

    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.set_pixel_size(16);
    if is_highlight {
        icon.add_css_class("menu-accent");
    } else if is_destructive {
        icon.add_css_class("sidebar-icon-trash");
    }
    hbox.append(&icon);

    let lbl = gtk4::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(true)
        .build();
    if is_highlight {
        lbl.add_css_class("menu-accent");
    }
    hbox.append(&lbl);

    if let Some(sc) = shortcut {
        let sc_lbl = gtk4::Label::builder()
            .label(sc)
            .xalign(1.0)
            .build();
        sc_lbl.add_css_class(if is_highlight { "shortcut-label-accent" } else { "shortcut-label" });
        hbox.append(&sc_lbl);
    }

    btn.set_child(Some(&hbox));
    (btn, hbox)
}

/// Shows a Nautilus-style right click context menu matching the stitch mockup.
/// `selected` holds every selected entry (right-click selects-under-cursor
/// first), so actions operate on the whole selection.
fn show_context_menu(
    target_widget: &impl IsA<gtk4::Widget>,
    x: f64,
    y: f64,
    selected: &[DirEntry],
    focused: &DirEntry,
    on_event: &Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>>,
    curr_path: &PathBuf,
    device: &str,
    local: bool,
) {
    let single = selected.len() == 1;
    let selection_label = if single {
        selected[0].name.clone()
    } else {
        format!("{} items", selected.len())
    };
    let emit = |cb: &Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>>, ev: BrowserEvent| {
        if let Some(f) = cb.borrow().as_ref() { f(ev); }
    };

    let popover = gtk4::Popover::new();
    popover.set_parent(target_widget);
    let rect = gdk4::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));

    let menu_box = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    menu_box.set_margin_top(6);
    menu_box.set_margin_bottom(6);
    menu_box.set_margin_start(6);
    menu_box.set_margin_end(6);

    // 1. Open (single selection only)
    let (open_btn, _) = create_menu_button(
        if focused.is_dir { "folder-open-symbolic" } else { "document-open-symbolic" },
        "Open",
        Some("Return"),
        false,
        false,
    );
    open_btn.set_sensitive(single);
    {
        let e_open = focused.clone();
        let on_ev = on_event.clone();
        let p = popover.clone();
        open_btn.connect_clicked(move |_| {
            p.popdown();
            if e_open.is_dir || e_open.is_symlink {
                emit(&on_ev, BrowserEvent::OpenDir(e_open.clone()));
            } else {
                emit(&on_ev, BrowserEvent::Download(vec![e_open.clone()]));
            }
        });
    }
    menu_box.append(&open_btn);

    // 2. Open With Other Application... (single)
    let (open_with_btn, _) = create_menu_button(
        "system-file-manager-symbolic",
        "Open With Other Application...",
        None,
        false,
        false,
    );
    open_with_btn.set_sensitive(single);
    {
        let fp = curr_path.join(&focused.name);
        let on_ev = on_event.clone();
        let p = popover.clone();
        open_with_btn.connect_clicked(move |_| {
            p.popdown();
            emit(&on_ev, BrowserEvent::OpenExternal(fp.clone()));
        });
    }
    menu_box.append(&open_with_btn);

    menu_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    // 3. Copy to Linux Home (ADB Pull) — the whole selection
    let (pull_btn, _) = create_menu_button(
        "folder-download-symbolic",
        "Copy to Linux Home (ADB Pull)",
        Some("Ctrl+Shift+C"),
        true,
        false,
    );
    pull_btn.add_css_class("menu-item-highlight");
    {
        let sel = selected.to_vec();
        let on_ev = on_event.clone();
        let p = popover.clone();
        pull_btn.connect_clicked(move |_| {
            p.popdown();
            emit(&on_ev, BrowserEvent::Download(sel.clone()));
        });
    }
    menu_box.append(&pull_btn);

    // 4. Push File to Device (ADB Push)...
    let (push_btn, _) = create_menu_button(
        "list-add-symbolic",
        "Push File to Device (ADB Push)...",
        Some("Ctrl+U"),
        false,
        false,
    );
    {
        let on_ev = on_event.clone();
        let p = popover.clone();
        push_btn.connect_clicked(move |_| {
            p.popdown();
            emit(&on_ev, BrowserEvent::Upload);
        });
    }
    menu_box.append(&push_btn);

    // 5. Install APK via ADB (only when the selection contains APKs)
    let (apk_btn, _) = create_menu_button(
        "application-x-executable-symbolic",
        "Install APK via ADB",
        None,
        false,
        false,
    );
    let apks: Vec<DirEntry> = selected.iter().filter(|e| e.name.to_lowercase().ends_with(".apk")).cloned().collect();
    apk_btn.set_sensitive(!apks.is_empty());
    {
        let on_ev = on_event.clone();
        let p = popover.clone();
        apk_btn.connect_clicked(move |_| {
            p.popdown();
            for apk in apks.clone() {
                emit(&on_ev, BrowserEvent::InstallApk(apk.clone()));
            }
        });
    }
    menu_box.append(&apk_btn);

    menu_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    // 6. Cut / 7. Copy — clipboard gets every selected path.
    let clipboard_paths = |entries: &[DirEntry], curr: &PathBuf| -> String {
        entries
            .iter()
            .map(|e| curr.join(&e.name).to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (cut_btn, _) = create_menu_button("edit-cut-symbolic", "Cut", Some("Ctrl+X"), false, false);
    {
        let paths = clipboard_paths(selected, curr_path);
        let p = popover.clone();
        cut_btn.connect_clicked(move |_| {
            p.popdown();
            if let Some(display) = gdk4::Display::default() {
                display.clipboard().set_text(&paths);
            }
        });
    }
    menu_box.append(&cut_btn);

    let (copy_btn, _) = create_menu_button("edit-copy-symbolic", "Copy", Some("Ctrl+C"), false, false);
    {
        let paths = clipboard_paths(selected, curr_path);
        let p = popover.clone();
        copy_btn.connect_clicked(move |_| {
            p.popdown();
            if let Some(display) = gdk4::Display::default() {
                display.clipboard().set_text(&paths);
            }
        });
    }
    menu_box.append(&copy_btn);

    // 8. Move to Trash / 9. Delete Permanently — the whole selection.
    let (trash_btn, _) = create_menu_button("user-trash-symbolic", "Move to Trash", Some("Delete"), false, false);
    {
        let sel = selected.to_vec();
        let label = selection_label.clone();
        let on_ev = on_event.clone();
        let p = popover.clone();
        let widget_for_trash = target_widget.clone().upcast::<gtk4::Widget>();
        let curr_for_trash = curr_path.clone();
        trash_btn.connect_clicked(move |_| {
            p.popdown();
            if local {
                // Real trash: recoverable via gio (goes to ~/.local/share/Trash).
                for e in &sel {
                    let target = curr_for_trash.join(&e.name);
                    let _ = std::process::Command::new("gio").args(["trash", &target.to_string_lossy()]).spawn();
                }
            } else {
                // The daemon delete is permanent; confirm first.
                show_delete_dialog(&widget_for_trash, &label, sel.clone(), &on_ev);
            }
        });
    }
    menu_box.append(&trash_btn);

    let (del_btn, _) = create_menu_button(
        "edit-delete-symbolic",
        "Delete Permanently from Android",
        Some("Shift+Del"),
        false,
        true,
    );
    {
        let sel = selected.to_vec();
        let label = selection_label.clone();
        let on_ev = on_event.clone();
        let p = popover.clone();
        let widget_for_del = target_widget.clone().upcast::<gtk4::Widget>();
        del_btn.connect_clicked(move |_| {
            p.popdown();
            show_delete_dialog(&widget_for_del, &label, sel.clone(), &on_ev);
        });
    }
    menu_box.append(&del_btn);

    menu_box.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    // 10. Compress... (single)
    let (comp_btn, _) = create_menu_button("package-x-generic-symbolic", "Compress...", None, false, false);
    comp_btn.set_sensitive(single);
    {
        let p = popover.clone();
        comp_btn.connect_clicked(move |_| {
            p.popdown();
        });
    }
    menu_box.append(&comp_btn);

    // 11. Open in Terminal (single)
    let (term_btn, _) = create_menu_button(
        "utilities-terminal-symbolic",
        "Open in Terminal (ADB Shell)",
        Some("Alt+T"),
        false,
        false,
    );
    term_btn.set_sensitive(single);
    {
        let p = if focused.is_dir { curr_path.join(&focused.name) } else { curr_path.clone() };
        let on_ev = on_event.clone();
        let p_pop = popover.clone();
        term_btn.connect_clicked(move |_| {
            p_pop.popdown();
            emit(&on_ev, BrowserEvent::OpenTerminal(p.clone()));
        });
    }
    menu_box.append(&term_btn);

    // 12. Properties (single)
    let (prop_btn, _) = create_menu_button("dialog-information-symbolic", "Properties", Some("Alt+Enter"), false, false);
    prop_btn.set_sensitive(single);
    {
        let e_prop = focused.clone();
        let p_pop = popover.clone();
        let widget_for_prop = target_widget.clone().upcast::<gtk4::Widget>();
        let full_p = curr_path.join(&focused.name);
        let dev_str = device.to_string();
        prop_btn.connect_clicked(move |_| {
            p_pop.popdown();
            show_properties_dialog(&widget_for_prop, &e_prop, &full_p, &dev_str);
        });
    }
    menu_box.append(&prop_btn);

    popover.set_child(Some(&menu_box));
    popover.popup();
}
fn show_new_folder_dialog(
    parent: &impl IsA<gtk4::Widget>,
    on_event: &Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>>,
) {
    let window = parent.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
    let dialog = gtk4::Dialog::builder()
        .title("New Folder")
        .transient_for(window.as_ref().unwrap())
        .modal(true)
        .build();

    let content_area = dialog.content_area();
    content_area.set_margin_start(16);
    content_area.set_margin_end(16);
    content_area.set_margin_top(16);
    content_area.set_margin_bottom(16);

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Folder name"));
    content_area.append(&entry);

    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
    let create_btn = dialog.add_button("Create", gtk4::ResponseType::Ok);
    create_btn.add_css_class("suggested-action");

    let on_ev = on_event.clone();
    dialog.connect_response(move |d, resp| {
        if resp == gtk4::ResponseType::Ok {
            let name = entry.text().trim().to_string();
            if !name.is_empty() {
                if let Some(cb) = on_ev.borrow().as_ref() {
                    cb(BrowserEvent::NewFolder(name));
                }
            }
        }
        d.close();
    });

    dialog.present();
}

fn show_delete_dialog(
    parent: &impl IsA<gtk4::Widget>,
    label: &str,
    entries: Vec<DirEntry>,
    on_event: &Rc<RefCell<Option<Box<dyn Fn(BrowserEvent)>>>>,
) {
    let window = parent.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
    let dialog = gtk4::Dialog::builder()
        .title("Delete Items")
        .transient_for(window.as_ref().unwrap())
        .modal(true)
        .build();

    let content_area = dialog.content_area();
    content_area.set_margin_start(16);
    content_area.set_margin_end(16);
    content_area.set_margin_top(16);
    content_area.set_margin_bottom(16);

    let text = gtk4::Label::builder()
        .label(format!("Permanently delete \"{}\"?", label))
        .wrap(true)
        .build();
    content_area.append(&text);

    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
    let del_btn = dialog.add_button("Delete", gtk4::ResponseType::Ok);
    del_btn.add_css_class("destructive-action");

    let on_ev = on_event.clone();
    dialog.connect_response(move |d, resp| {
        if resp == gtk4::ResponseType::Ok {
            if let Some(cb) = on_ev.borrow().as_ref() {
                cb(BrowserEvent::Delete(entries.clone()));
            }
        }
        d.close();
    });

    dialog.present();
}

fn show_properties_dialog(
    parent: &impl IsA<gtk4::Widget>,
    entry: &DirEntry,
    full_path: &PathBuf,
    device: &str,
) {
    let window = parent.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
    let dialog = gtk4::Dialog::builder()
        .title(&format!("{} Properties", entry.name))
        .transient_for(window.as_ref().unwrap())
        .modal(true)
        .build();

    let content_area = dialog.content_area();
    content_area.set_margin_start(16);
    content_area.set_margin_end(16);
    content_area.set_margin_top(16);
    content_area.set_margin_bottom(16);

    let group = adw::PreferencesGroup::new();

    let row_name = adw::ActionRow::builder().title("Name").subtitle(&entry.name).build();
    let row_type = adw::ActionRow::builder().title("Type").subtitle(entry.file_type_desc()).build();
    let row_size = adw::ActionRow::builder().title("Size").subtitle(if entry.is_dir { "—".to_string() } else { format!("{} ({} bytes)", entry.display_size(), entry.size) }).build();
    let row_path = adw::ActionRow::builder().title("Location").subtitle(full_path.to_string_lossy().as_ref()).build();
    let row_date = adw::ActionRow::builder().title("Modified").subtitle(&entry.display_date()).build();
    let row_mode = adw::ActionRow::builder().title("Permissions").subtitle(format!("{:#o}", entry.mode & 0o7777)).build();

    group.add(&row_name);
    group.add(&row_type);
    group.add(&row_size);
    group.add(&row_path);
    group.add(&row_date);
    group.add(&row_mode);

    content_area.append(&group);
    dialog.add_button("Close", gtk4::ResponseType::Close);
    dialog.connect_response(|d, _| d.close());
    dialog.present();
}