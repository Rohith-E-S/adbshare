//! The application root: top bar, sidebar, and everything that talks to the
//! daemon.
//!
//! The GTK build had one 3,500-line `app.rs` that owned the whole widget tree
//! and dispatched every action. GPUI makes a better seam available: the file
//! browser is its own entity and emits [`BrowserEvent`], so this module is just
//! the conductor — it renders the chrome, owns the state the chrome needs
//! (selected device, jobs, dialogs, menus), and turns events into D-Bus or local
//! filesystem work.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Entity, FocusHandle, IntoElement,
    KeyBinding, MouseButton, MouseMoveEvent, Pixels, Point, Render, Subscription, Window, actions,
    div, px, rgba,
};

use crate::browser::{Browser, BrowserEvent, CONTEXT_BAR_H, ViewMode};
use crate::clipboard;
use crate::daemon;
use crate::dialogs::{
    self, Dialog, DialogResult, MessageTone, WifiStep, default_save_dir, validate_name,
};
use crate::filechooser::{self, Pick};
use crate::icons::{self, names};
use crate::localfs;
use crate::menu::{self, MenuItem};
use crate::prefs::{Preferences, SIDEBAR_RANGE};
use crate::protocol::{
    ClipboardFiles, DeviceEntry, DirEntry, JobInfo, LOCAL_DEVICE, SortKey, format_capacity,
    tree_result_message,
};
use crate::textinput::{TextField, TextFieldEvent};
use crate::theme::{self, Themed};
use crate::toast::{ActionId, ToastStack, ToastTone};
use crate::ui;

/// How often the device list is refreshed.
const DEVICE_POLL: Duration = Duration::from_secs(3);
/// How often the transfer queue is refreshed. Much faster than the device poll
/// because progress has to look live.
const JOB_POLL: Duration = Duration::from_millis(600);
const SIDEBAR_MIN: f32 = SIDEBAR_RANGE.0;
const SIDEBAR_MAX: f32 = SIDEBAR_RANGE.1;
/// Window widths at which the chrome sheds parts, matching the old breakpoints.
const COMPACT_BELOW: f32 = 1000.0;
const NARROW_BELOW: f32 = 760.0;

actions!(
    app,
    [
        DismissOverlays,
        ShowAbout,
        ShowDiagnostics,
        ConnectWifi,
        ToggleSidebar,
        ToggleShowHidden,
        SelectAllEntries,
    ]
);

/// The app root's own key bindings, as `(keys, factory)`.
///
/// Data rather than a bare call so the shortcuts dialog can be checked against
/// what is actually bound; see `browser::BINDINGS`.
pub const BINDINGS: &[crate::browser::Binding] = &[
    ("f9", || KeyBinding::new("f9", ToggleSidebar, None)),
    ("secondary-comma", || {
        KeyBinding::new("secondary-comma", ShowDiagnostics, None)
    }),
    ("shift-f10", || {
        KeyBinding::new("shift-f10", ConnectWifi, None)
    }),
];

/// Register the app-level key bindings. Called once from `main`.
pub fn install_key_bindings(cx: &mut App) {
    cx.bind_keys(BINDINGS.iter().map(|(_, make)| make()));
}

/// What the user has chosen to browse.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selection {
    Device(String),
    Local(PathBuf),
}

/// Where the right-click menu appears.
#[derive(Debug, Clone)]
struct ContextMenuState {
    position: Point<Pixels>,
}

/// How much of the chrome fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Everything, including the operations and view capsules.
    Full,
    /// Sidebar plus browser; the secondary capsules are hidden.
    Compact,
    /// Browser only.
    Narrow,
}

impl Layout {
    /// Pick a layout for a window width.
    fn for_width(width: f32) -> Self {
        if width < NARROW_BELOW {
            Layout::Narrow
        } else if width < COMPACT_BELOW {
            Layout::Compact
        } else {
            Layout::Full
        }
    }

    fn shows_ops(self) -> bool {
        self == Layout::Full
    }
}

/// The phone folder shortcuts offered in the sidebar.
const PHONE_PLACES: &[(&str, &str, &str)] = &[
    ("Internal storage", names::PHONE, "/sdcard"),
    ("Download", names::FOLDER_DOWNLOAD, "/sdcard/Download"),
    ("Camera", names::CAMERA_PHOTO, "/sdcard/DCIM"),
    ("Pictures", names::FOLDER_PICTURES, "/sdcard/Pictures"),
    ("Music", names::FOLDER_MUSIC, "/sdcard/Music"),
    ("Documents", names::FOLDER_DOCUMENTS, "/sdcard/Documents"),
];

pub struct AdbShareApp {
    browser: Entity<Browser>,

    // ── What is selected ───────────────────────────────────────────────────
    selection: Option<Selection>,
    devices: Vec<DeviceEntry>,

    // ── Transfers ──────────────────────────────────────────────────────────
    jobs: Vec<JobInfo>,

    // ── Chrome state ───────────────────────────────────────────────────────
    layout: Layout,
    sidebar_visible: bool,
    sidebar_width: f32,
    /// True while the sidebar divider is being dragged.
    resizing_sidebar: bool,
    transfers_open: bool,
    overflow_open: bool,
    context_menu: Option<ContextMenuState>,

    // ── Dialog state ───────────────────────────────────────────────────────
    dialog: Dialog,
    /// The field shared by the New Folder and Rename dialogs.
    name_field: Entity<TextField>,
    search_field: Entity<TextField>,
    path_field: Entity<TextField>,
    pair_address_field: Entity<TextField>,
    pair_code_field: Entity<TextField>,

    // ── Clipboard ──────────────────────────────────────────────────────────
    clipboard: Option<ClipboardFiles>,

    toasts: ToastStack,
    /// Consecutive failed device polls. The first failure is usually just the
    /// daemon still starting; by the second, the user needs telling.
    daemon_failures: u32,
    /// Set while the daemon is unreachable, so the sidebar can say so instead of
    /// implying no phone is plugged in.
    daemon_error: Option<String>,
    focus: FocusHandle,
    /// The last preferences written out, so a save only happens on a real
    /// change.
    saved: Preferences,
    subscriptions: Vec<Subscription>,
}

impl AdbShareApp {
    /// Build the root view. Passed straight to `Application::open_window`.
    pub fn build(_window: &mut Window, cx: &mut App) -> Entity<Self> {
        let saved = Preferences::load();
        let view = Self::new_entity(_window, cx, &saved);
        // The browser's own view preferences live with the rest of them.
        view.update(cx, |this, cx| {
            let view_prefs = saved.view();
            let browser = this.browser.clone();
            browser.update(cx, |b, cx| b.apply_preferences(view_prefs, cx));
        });
        Self::wire_up(&view, cx);
        view
    }

    /// The same, minus `wire_up`, for tests that drive the view directly.
    pub fn new_entity(_window: &mut Window, cx: &mut App, saved: &Preferences) -> Entity<Self> {
        cx.new(|cx| Self::new(cx, saved))
    }

    /// Construct the view with the given preferences already applied.
    ///
    /// Split out from [`Self::build`] so the state can be exercised without
    /// opening a window, which is what the daemon-failure tests do.
    fn new(cx: &mut Context<Self>, saved: &Preferences) -> Self {
        Self {
            browser: cx.new(Browser::new),
            selection: None,
            devices: Vec::new(),
            jobs: Vec::new(),
            layout: Layout::Full,
            sidebar_visible: saved.sidebar_visible,
            sidebar_width: saved.sidebar_width,
            resizing_sidebar: false,
            transfers_open: false,
            overflow_open: false,
            context_menu: None,
            dialog: Dialog::None,
            name_field: cx.new(|cx| TextField::new(cx, "Name")),
            search_field: cx.new(|cx| TextField::new(cx, "Search this folder")),
            path_field: cx.new(|cx| TextField::new(cx, "/sdcard").monospace()),
            pair_address_field: cx.new(|cx| TextField::new(cx, "192.168.1.20:37001").monospace()),
            pair_code_field: cx.new(|cx| TextField::new(cx, "000000").monospace()),
            clipboard: None,
            toasts: ToastStack::default(),
            daemon_failures: 0,
            daemon_error: None,
            focus: cx.focus_handle(),
            saved: saved.clone(),
            subscriptions: Vec::new(),
        }
    }

    /// The preferences that describe the current view.
    fn current_preferences(&self, cx: &gpui::App) -> Preferences {
        let browser = self.browser.read(cx);
        Preferences {
            sidebar_width: self.sidebar_width,
            sidebar_visible: self.sidebar_visible,
            zoom: browser.zoom(),
            list_view: browser.view_mode() == ViewMode::List,
            show_hidden: browser.show_hidden(),
            sort_key: browser.sort_key(),
            sort_descending: browser.sort_descending(),
        }
    }

    /// Write the preferences out if they changed since the last write.
    ///
    /// This rides the existing poll tick rather than a shutdown hook, which
    /// does not fire when the app quits, and it covers every path that can
    /// change a preference — menu item, keybinding or drag — without any event
    /// plumbing between the views.
    fn persist_if_changed(&mut self, cx: &mut Context<Self>) {
        let current = self.current_preferences(cx);
        if current == self.saved {
            return;
        }
        current.save();
        self.saved = current;
    }

    /// Subscribe to child entities and start the poll loops.
    fn wire_up(view: &Entity<Self>, cx: &mut App) {
        // Browser events are the app's main input path.
        let me = view.clone();
        let browser = view.read(cx).browser.clone();
        let sub = cx.subscribe(&browser, move |_browser, event: &BrowserEvent, cx| {
            me.update(cx, |this, cx| this.on_browser_event(event, cx));
        });
        view.update(cx, |this, _cx| this.subscriptions.push(sub));

        // The search field drives the browser's filter.
        let me = view.clone();
        let search = view.read(cx).search_field.clone();
        let sub = cx.subscribe(&search, move |_field, event: &TextFieldEvent, cx| {
            if let TextFieldEvent::Changed(query) = event {
                let query = query.to_string();
                me.update(cx, |this, cx| {
                    this.browser
                        .update(cx, |b, cx| b.set_search_query(query, cx));
                });
            }
        });
        view.update(cx, |this, _cx| this.subscriptions.push(sub));

        // Ctrl+L: the path field starts out holding the current path, so Enter
        // just works.
        let me = view.clone();
        let path_field = view.read(cx).path_field.clone();
        let sub = cx.subscribe(&path_field, move |_field, event: &TextFieldEvent, cx| {
            if let TextFieldEvent::Submitted(path) = event {
                let path = PathBuf::from(path.to_string());
                me.update(cx, |this, cx| {
                    this.browser.update(cx, |b, _| b.navigate(path.clone()));
                    this.on_browser_event(&BrowserEvent::OpenedDirectory(path.clone()), cx);
                });
            }
        });
        view.update(cx, |this, _cx| this.subscriptions.push(sub));

        // The pairing address advances the flow; the code submits it.
        let me = view.clone();
        let address = view.read(cx).pair_address_field.clone();
        let sub = cx.subscribe(&address, move |_field, event: &TextFieldEvent, cx| {
            if let TextFieldEvent::Submitted(value) = event {
                let value = value.to_string();
                me.update(cx, |this, cx| {
                    this.pairing_step(WifiStep::Pairing { address: value });
                    cx.notify();
                });
            }
        });
        view.update(cx, |this, _cx| this.subscriptions.push(sub));

        let me = view.clone();
        let code = view.read(cx).pair_code_field.clone();
        let sub = cx.subscribe(&code, move |_field, event: &TextFieldEvent, cx| {
            if let TextFieldEvent::Submitted(value) = event {
                let address = me.read(cx).pair_address_field.read(cx).value().to_string();
                let code = value.to_string();
                me.update(cx, |this, cx| {
                    this.on_dialog_result(&DialogResult::PairCode { address, code }, cx);
                });
            }
        });
        view.update(cx, |this, _cx| this.subscriptions.push(sub));

        // New Folder and Rename both submit through the shared name field.
        let me = view.clone();
        let name = view.read(cx).name_field.clone();
        let sub = cx.subscribe(&name, move |_field, event: &TextFieldEvent, cx| {
            if let TextFieldEvent::Submitted(value) = event {
                let value = value.to_string();
                let result = match me.read(cx).name_field_purpose() {
                    Some(NamePurpose::CreateFolder) => DialogResult::CreateFolder(value),
                    Some(NamePurpose::Rename(entry)) => DialogResult::Rename(entry, value),
                    None => return,
                };
                me.update(cx, |this, cx| this.on_dialog_result(&result, cx));
            }
        });
        view.update(cx, |this, _cx| this.subscriptions.push(sub));

        Self::start_polls(view, cx);
    }

    /// Start the device and job poll loops.
    ///
    /// The daemon exposes no D-Bus signals, so this is the same polling design
    /// as the GTK build: a slow device sweep and a fast job sweep.
    fn start_polls(view: &Entity<Self>, cx: &mut App) {
        let devices = view.clone();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(DEVICE_POLL).await;
                let result = daemon::list_devices().await;
                devices
                    .update(cx, |this, cx| this.on_devices_result(result, cx))
                    .ok();
            }
        })
        .detach();

        let jobs = view.clone();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(JOB_POLL).await;
                let result = daemon::list_jobs().await;
                jobs.update(cx, |this, cx| this.on_jobs_result(result, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Run blocking work on a worker thread, then apply the result on the UI
    /// thread.
    ///
    /// Local filesystem calls are synchronous and can be slow on a cold network
    /// mount, so they must not run on the frame path.
    fn off_thread<F, R>(
        &self,
        cx: &mut Context<Self>,
        work: F,
        apply: impl FnOnce(R, &mut Context<Self>) + 'static,
    ) where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = cx.background_executor().spawn(async move { work() }).await;
            me.update(cx, |_this, cx| apply(result, cx)).ok();
        })
        .detach();
    }

    // ── Poll results ───────────────────────────────────────────────────────

    fn on_devices_result(&mut self, result: Result<Vec<String>, String>, cx: &mut Context<Self>) {
        let serials = match result {
            Ok(serials) => {
                self.daemon_failures = 0;
                self.daemon_error = None;
                serials
            }
            Err(err) => {
                self.daemon_failures += 1;
                let had_devices = !self.devices.is_empty();
                self.devices.clear();
                self.selection = None;
                self.browser.update(cx, |b, cx| b.set_idle(cx));

                // A single failure is usually just the daemon still starting, so
                // it is not worth a message. Once it has failed twice the user
                // needs to be told, because an empty sidebar otherwise reads as
                // "no phone plugged in" and sends them looking for the cable.
                let repeated = self.daemon_failures >= 2;
                if repeated || had_devices {
                    // The raw D-Bus error tells the user nothing; the sidebar
                    // keeps it for detail but leads with what to do about it.
                    let advice = daemon::explain(&err);
                    self.daemon_error = Some(advice);
                    self.toasts.push_with_action(
                        "Cannot reach adb-daemon",
                        ToastTone::Error,
                        Some(("Diagnose".into(), ActionId::CopyDiagnostics)),
                    );
                }
                cx.notify();
                return;
            }
        };

        // Announce arrivals and departures before rebuilding the list, so the
        // toast reads sensibly.
        let before: Vec<String> = self.devices.iter().map(|d| d.serial.clone()).collect();
        for serial in &serials {
            if !before.contains(serial) {
                let name = self
                    .devices
                    .iter()
                    .find(|d| &d.serial == serial)
                    .map(|d| d.display_name().to_string())
                    .unwrap_or_else(|| serial.clone());
                self.toasts
                    .push(format!("{name} connected"), ToastTone::Success);
            }
        }
        for serial in &before {
            if !serials.contains(serial) {
                self.toasts
                    .push(format!("{serial} disconnected"), ToastTone::Warning);
            }
        }

        // Fan out concurrently. The daemon answers `device_info` per serial, so
        // awaiting them in sequence delayed the sidebar by one round trip per
        // connected device.
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let entries: Vec<DeviceEntry> =
                futures::future::join_all(serials.iter().map(|serial| {
                    let serial = serial.clone();
                    async move {
                        match daemon::device_info(&serial).await {
                            Ok(info) => info.into_entry(),
                            Err(_) => DeviceEntry::fallback(&serial),
                        }
                    }
                }))
                .await;
            me.update(cx, |this, cx| this.set_devices(entries, cx)).ok();
        })
        .detach();
    }

    fn set_devices(&mut self, devices: Vec<DeviceEntry>, cx: &mut Context<Self>) {
        // If the selected device vanished, drop the selection rather than
        // silently moving the user to a different phone.
        if let Some(Selection::Device(serial)) = &self.selection
            && !devices.iter().any(|d| &d.serial == serial)
        {
            self.toasts
                .push("The selected phone disconnected", ToastTone::Warning);
            self.selection = None;
            self.browser.update(cx, |b, cx| b.set_idle(cx));
        }
        // The list is polled every three seconds and rarely differs. Re-rendering
        // the whole window on an identical result is pure waste, and it is the
        // reason the sidebar used to repaint while the user was reading a file
        // name.
        if devices == self.devices {
            return;
        }
        self.devices = devices;
        cx.notify();
    }

    fn on_jobs_result(&mut self, result: Result<Vec<JobInfo>, String>, cx: &mut Context<Self>) {
        let Ok(jobs) = result else {
            // A transient bus error is not worth interrupting the user for; the
            // next tick picks up where this left off.
            return;
        };
        // Toasts expire on the same tick, so no extra timer is needed.
        let expired = self.toasts.prune();
        self.persist_if_changed(cx);

        // The queue only changes while something is moving. Re-rendering the
        // whole window on every 600ms tick regardless made the UI repaint
        // roughly twice a second for nothing, which shows up as churn in hover
        // state and scroll position.
        if jobs == self.jobs {
            if expired {
                cx.notify();
            }
            return;
        }

        self.jobs = jobs;
        let active: Vec<&JobInfo> = self.jobs.iter().filter(|job| job.is_active()).collect();
        let active_count = active.len();
        // With nothing running there is nothing to pause, so this must be false:
        // `all` over an empty iterator is true, which would claim the queue was
        // paused and blank the status line.
        let paused = !active.is_empty() && active.iter().all(|job| job.state == "Paused");
        self.browser.update(cx, |b, cx| {
            b.set_job_counts(active_count, self.jobs.len(), paused, cx)
        });
        cx.notify();
    }

    // ── Selection ───────────────────────────────────────────────────────────

    fn select_device(&mut self, serial: &str, cx: &mut Context<Self>) {
        let display = self
            .devices
            .iter()
            .find(|d| d.serial == serial)
            .map(|d| d.display_name().to_string())
            .unwrap_or_else(|| serial.to_string());

        self.selection = Some(Selection::Device(serial.to_string()));
        self.context_menu = None;
        self.browser.update(cx, |b, cx| {
            b.set_device(serial, &display, cx);
        });
        self.fetch_dir(serial.to_string(), cx);
        self.fetch_mountpoint(serial.to_string(), cx);
    }

    fn select_local(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.selection = Some(Selection::Local(path.to_path_buf()));
        self.context_menu = None;
        self.browser.update(cx, |b, cx| b.set_local(path, cx));
        self.list_local(path.to_path_buf(), cx);
    }

    /// The serial to address the daemon with, or `None` in local mode.
    fn target_device(&self) -> Option<String> {
        match &self.selection {
            Some(Selection::Device(serial)) => Some(serial.clone()),
            _ => None,
        }
    }

    // ── Listings ───────────────────────────────────────────────────────────

    fn fetch_dir(&mut self, device: String, cx: &mut Context<Self>) {
        let path = self.browser.read(cx).path().to_path_buf();
        self.fetch_dir_at(device, path, cx);
    }

    fn fetch_dir_at(&mut self, device: String, path: PathBuf, cx: &mut Context<Self>) {
        let browser = self.browser.clone();
        cx.spawn(async move |_this, cx| {
            let result = daemon::list_dir(&device, &path.to_string_lossy()).await;
            browser
                .update(cx, |b, cx| {
                    // A listing that arrives after the user has moved on is
                    // dropped, so a slow device cannot overwrite a newer view.
                    if b.device() != Some(device.as_str()) || b.path() != path {
                        return;
                    }
                    match result {
                        Ok(entries) => b.set_entries(entries, cx),
                        Err(err) => b.set_error(format!("Could not read {path:?}: {err}"), cx),
                    }
                })
                .ok();
        })
        .detach();
    }

    /// List a directory on the local disk.
    ///
    /// `std::fs` is synchronous and a cold network mount can block for seconds,
    /// so the read happens on a worker thread and the result is applied only if
    /// the browser is still looking at that directory.
    fn list_local(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let browser = self.browser.clone();
        let wanted = path.clone();
        self.off_thread(
            cx,
            move || localfs::list_dir(&path),
            move |result, cx| {
                browser.update(cx, |b, cx| {
                    if b.path() != wanted || !b.is_local() {
                        return;
                    }
                    match result {
                        Ok(entries) => b.set_entries(entries, cx),
                        Err(err) => b.set_error(err, cx),
                    }
                });
            },
        );
    }

    /// Re-read whatever the browser is currently showing.
    fn refresh_current(&mut self, cx: &mut Context<Self>) {
        if self.browser.read(cx).is_local() {
            let path = self.browser.read(cx).path().to_path_buf();
            self.list_local(path, cx);
        } else if let Some(device) = self.target_device() {
            self.fetch_dir(device, cx);
        }
    }

    fn fetch_mountpoint(&mut self, device: String, cx: &mut Context<Self>) {
        let browser = self.browser.clone();
        cx.spawn(async move |_this, cx| {
            let mount = daemon::mountpoint_for(&device)
                .await
                .ok()
                .filter(|m| !m.is_empty());
            browser.update(cx, |b, cx| b.set_fuse_mount(mount, cx)).ok();
        })
        .detach();
    }

    // ── Browser events ─────────────────────────────────────────────────────

    fn on_browser_event(&mut self, event: &BrowserEvent, cx: &mut Context<Self>) {
        match event {
            BrowserEvent::OpenedDirectory(path) => {
                if self.browser.read(cx).is_local() {
                    self.list_local(path.clone(), cx);
                } else if let Some(device) = self.target_device() {
                    self.fetch_dir_at(device, path.clone(), cx);
                }
            }
            BrowserEvent::Navigate(path) => {
                self.browser.update(cx, |b, _| b.navigate(path.clone()));
                if self.browser.read(cx).is_local() {
                    self.list_local(path.clone(), cx);
                } else if let Some(device) = self.target_device() {
                    self.fetch_dir_at(device, path.clone(), cx);
                }
            }
            BrowserEvent::Up => {
                let parent = self.browser.read(cx).path().parent().map(Path::to_path_buf);
                if let Some(parent) = parent {
                    self.browser.update(cx, |b, _| b.navigate(parent.clone()));
                    if self.browser.read(cx).is_local() {
                        self.list_local(parent.clone(), cx);
                    } else if let Some(device) = self.target_device() {
                        self.fetch_dir_at(device, parent, cx);
                    }
                }
            }
            BrowserEvent::Refresh => self.refresh_current(cx),
            BrowserEvent::Open(entry) => self.open_entry(entry.clone(), cx),
            BrowserEvent::InstallApk(entry) => self.install_apk(entry.clone(), cx),
            BrowserEvent::NewFolder(_) => {
                self.name_field.update(cx, |f, cx| {
                    f.set_value("", cx);
                    f.select_everything(cx);
                });
                self.dialog = Dialog::NewFolder {
                    field: self.name_field.clone(),
                };
                cx.notify();
            }
            BrowserEvent::Rename(entry, current) => {
                let field = cx.new(|cx| TextField::with_value(cx, current.clone(), "New name"));
                self.dialog = Dialog::Rename {
                    entry: entry.clone(),
                    field,
                };
                cx.notify();
            }
            BrowserEvent::Trash(entries) => self.trash_entries(entries.clone(), cx),
            BrowserEvent::DeletePermanently(entries) => {
                self.dialog = Dialog::ConfirmDelete {
                    label: describe_selection(entries),
                    entries: entries.clone(),
                    trash: false,
                };
                cx.notify();
            }
            BrowserEvent::Copy(entries) => self.copy(entries.clone(), cx),
            BrowserEvent::Paste => self.paste(cx),
            BrowserEvent::TogglePauseTransfers => self.toggle_pause(cx),
            BrowserEvent::CancelTransfers => self.cancel_transfers(cx),
            BrowserEvent::ContextMenu { position, .. } => {
                self.context_menu = Some(ContextMenuState {
                    position: *position,
                });
                cx.notify();
            }
        }
    }

    fn open_entry(&mut self, entry: DirEntry, cx: &mut Context<Self>) {
        // A device file can only be opened through its FUSE mount; without one
        // there is nothing for the desktop to open.
        if let Some(local) = self.browser.read(cx).local_path_of(&entry) {
            if let Err(err) = localfs::open_external(&local) {
                self.toasts.push(err, ToastTone::Error);
            }
        } else if !self.browser.read(cx).is_local() {
            self.toasts.push(
                "Opening a file from the phone needs a FUSE mount — use the transfers \
                 list to save it instead.",
                ToastTone::Warning,
            );
        }
        cx.notify();
    }

    fn copy(&mut self, entries: Vec<DirEntry>, cx: &mut Context<Self>) {
        let from_dir = self.browser.read(cx).path().to_path_buf();
        let from_local = self.browser.read(cx).is_local();
        let device = self.target_device();
        self.clipboard = Some(ClipboardFiles {
            from_local,
            device,
            from_dir: from_dir.clone(),
            entries: entries.clone(),
        });
        // Mirror to the system clipboard so the paths are pasteable elsewhere.
        clipboard::mirror_to_system(cx, &entries, &from_dir);
        self.toasts
            .push(format!("Copied {} item(s)", entries.len()), ToastTone::Info);
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.clipboard.clone() else {
            // Nothing copied in-app: accept a path list from the system
            // clipboard, which is what GTK offered and what a user pasting from
            // a terminal expects.
            let paths = clipboard::paths_from_system(cx);
            if paths.is_empty() {
                self.toasts.push("Nothing copied yet", ToastTone::Warning);
                cx.notify();
                return;
            }
            let base = paths[0]
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("/"));
            let entries = paths
                .iter()
                .map(|path| {
                    let meta = std::fs::metadata(path).ok();
                    DirEntry {
                        name: file_name_of(path),
                        is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                        is_symlink: false,
                        size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                        // A path list carries no mode or mtime; the properties
                        // dialog shows a dash rather than a wrong value.
                        mode: 0,
                        mtime: 0,
                    }
                })
                .collect();
            self.clipboard = Some(ClipboardFiles {
                from_local: true,
                device: None,
                from_dir: base,
                entries,
            });
            self.paste(cx);
            return;
        };
        let target_dir = self.browser.read(cx).path().to_path_buf();
        let target_device = self.target_device();

        let destinations = match clipboard::resolve_destination(
            &snapshot,
            &target_dir,
            target_device.as_deref(),
        ) {
            Ok(paths) => paths,
            Err(reason) => {
                self.toasts.push(reason, ToastTone::Error);
                cx.notify();
                return;
            }
        };

        let sources = clipboard::absolute_paths(&snapshot.entries, &snapshot.from_dir);
        // A single directory pastes as a recursive tree; anything else goes
        // through the file queue.
        if sources.len() == 1 && snapshot.entries.first().is_some_and(|e| e.looks_like_dir()) {
            self.paste_tree(
                &snapshot,
                sources[0].clone(),
                destinations[0].clone(),
                target_device,
                cx,
            );
            return;
        }

        // A file pasted into the phone it came from is copied on the device, so
        // nothing has to round-trip through the host.
        if let (Some(Selection::Device(device)), Some(origin)) =
            (self.selection.clone(), snapshot.device.clone())
            && device == origin
        {
            for (source, destination) in sources.iter().zip(destinations.iter()) {
                self.copy_on_device(device.clone(), source, destination, cx);
            }
            cx.notify();
            return;
        }

        for (source, destination) in sources.iter().zip(destinations.iter()) {
            self.enqueue_one(
                source.clone(),
                destination.clone(),
                target_device.clone(),
                cx,
            );
        }
        cx.notify();
    }

    /// Queue a recursive copy or transfer of one directory.
    fn paste_tree(
        &mut self,
        snapshot: &ClipboardFiles,
        source: PathBuf,
        destination: PathBuf,
        target_device: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match (&snapshot.from_local, &target_device) {
            (true, _) => {
                // Local to local: do it here, the daemon is not involved.
                match localfs::copy_tree(&source, &destination) {
                    Ok(count) => self.toasts.push(
                        format!("Copied {count} file(s) to {}", destination.display()),
                        ToastTone::Success,
                    ),
                    Err(err) => self
                        .toasts
                        .push(format!("Could not copy: {err}"), ToastTone::Error),
                }
            }
            (false, Some(device)) if source.is_absolute() => {
                // Device to itself: let the device copy, so nothing round-trips
                // through the host.
                let device = device.clone();
                let source = source.to_string_lossy().to_string();
                let parent = destination
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| "/".to_string());
                let me = cx.entity();
                cx.spawn(async move |_this, cx| {
                    let result = daemon::copy_tree(&device, &source, &parent).await;
                    me.update(cx, |this, cx| this.on_tree_result("Copy", result, cx))
                        .ok();
                })
                .detach();
            }
            (false, Some(device)) => {
                self.enqueue_tree(source, destination, device.clone(), cx);
            }
            (false, None) => {
                self.toasts.push(
                    "Those files are on a phone; open one to save it to this computer.",
                    ToastTone::Warning,
                );
                cx.notify();
            }
        }
    }

    /// Queue one file or directory, choosing push or pull by where it came from.
    fn enqueue_one(
        &mut self,
        source: PathBuf,
        destination: PathBuf,
        target_device: Option<String>,
        cx: &mut Context<Self>,
    ) {
        match (self.selection.clone(), target_device) {
            (Some(Selection::Device(device)), _) => {
                self.enqueue_push(device, source, destination, cx)
            }
            (Some(Selection::Local(_)), None) => match localfs::copy_file(&source, &destination) {
                Ok(()) => self
                    .toasts
                    .push(format!("Copied {}", source.display()), ToastTone::Success),
                Err(err) => self
                    .toasts
                    .push(format!("Could not copy: {err}"), ToastTone::Error),
            },
            (Some(Selection::Local(_)), Some(device)) => {
                self.enqueue_pull(device, source, destination, cx)
            }
            (None, _) => {}
        }
    }

    /// Copy a file on the device itself, without staging it through the queue.
    fn copy_on_device(
        &mut self,
        device: String,
        source: &Path,
        destination: &Path,
        cx: &mut Context<Self>,
    ) {
        let (source, destination) = (
            source.to_string_lossy().to_string(),
            destination.to_string_lossy().to_string(),
        );
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::copy_file(&device, &source, &destination).await;
            me.update(cx, |this, cx| {
                match &result {
                    Ok(()) => this
                        .toasts
                        .push(format!("Copied to {destination}"), ToastTone::Success),
                    Err(err) => this
                        .toasts
                        .push(format!("Could not copy: {err}"), ToastTone::Error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn enqueue_push(
        &mut self,
        device: String,
        local: PathBuf,
        remote: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let policy = daemon::transfer_policy(cx);
        let me = cx.entity();
        let label = remote.display().to_string();
        cx.spawn(async move |_this, cx| {
            let result = daemon::enqueue_push(
                &device,
                &local.to_string_lossy(),
                &remote.to_string_lossy(),
                policy,
            )
            .await;
            me.update(cx, |this, cx| {
                match result {
                    Ok(_) => this.toasts.push(format!("Queued {label}"), ToastTone::Info),
                    Err(err) => this.toasts.push_with_action(
                        format!("Could not queue {label}: {err}"),
                        ToastTone::Error,
                        Some(("Open downloads".into(), ActionId::OpenDownloads)),
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn enqueue_pull(
        &mut self,
        device: String,
        remote: PathBuf,
        local: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let policy = daemon::transfer_policy(cx);
        let me = cx.entity();
        let label = remote.display().to_string();
        cx.spawn(async move |_this, cx| {
            let result = daemon::enqueue_pull(
                &device,
                &remote.to_string_lossy(),
                &local.to_string_lossy(),
                policy,
            )
            .await;
            me.update(cx, |this, cx| {
                match result {
                    Ok(_) => this.toasts.push(format!("Queued {label}"), ToastTone::Info),
                    Err(err) => this.toasts.push_with_action(
                        format!("Could not queue {label}: {err}"),
                        ToastTone::Error,
                        Some(("Retry".into(), ActionId::RetryFailed)),
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn enqueue_tree(
        &mut self,
        local: PathBuf,
        remote: PathBuf,
        device: String,
        cx: &mut Context<Self>,
    ) {
        let policy = daemon::transfer_policy(cx);
        let me = cx.entity();
        let label = remote.display().to_string();
        cx.spawn(async move |_this, cx| {
            let result = daemon::enqueue_tree_push(
                &device,
                &local.to_string_lossy(),
                &remote.to_string_lossy(),
                policy,
            )
            .await;
            me.update(cx, |this, _cx| match result {
                Ok(outcome) => match tree_result_message("Upload", &outcome) {
                    Ok(msg) => this.toasts.push(msg, ToastTone::Success),
                    Err(msg) => this.toasts.push(msg, ToastTone::Error),
                },
                Err(err) => this
                    .toasts
                    .push(format!("Could not queue {label}: {err}"), ToastTone::Error),
            })
            .ok();
        })
        .detach();
    }

    // ── Transfers ──────────────────────────────────────────────────────────

    fn pick_for_upload(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.target_device() else {
            self.dialog = Dialog::Message {
                title: "No phone selected".into(),
                body: "Pick a device in the sidebar first, then send files to it.".into(),
                tone: MessageTone::Info,
                extra: None,
            };
            cx.notify();
            return;
        };
        let destination = self.browser.read(cx).path().to_path_buf();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let Some(paths) = filechooser::pick(Pick::Files, "Send to the phone")
                .await
                .ok()
                .flatten()
            else {
                return;
            };
            me.update(cx, |this, cx| {
                for path in paths {
                    let remote = destination.join(file_name_of(&path));
                    this.enqueue_push(device.clone(), path, remote, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn pick_folder_for_upload(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.target_device() else {
            self.toasts
                .push("Select a phone before sending a folder", ToastTone::Warning);
            return;
        };
        let destination = self.browser.read(cx).path().to_path_buf();
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let Some(paths) = filechooser::pick(Pick::Folder, "Send a folder to the phone")
                .await
                .ok()
                .flatten()
            else {
                return;
            };
            me.update(cx, |this, cx| {
                for path in paths {
                    let remote = destination.join(file_name_of(&path));
                    this.enqueue_tree(path, remote, device.clone(), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn download(&mut self, entries: Vec<DirEntry>, cx: &mut Context<Self>) {
        let Some(device) = self.target_device() else {
            self.toasts.push(
                "Select a phone before saving to the computer",
                ToastTone::Warning,
            );
            cx.notify();
            return;
        };
        // Resolve every source path now: the browser's path can change while
        // the picker is open.
        let sources: Vec<(String, String)> = entries
            .iter()
            .map(|entry| {
                let full = self.browser.read(cx).full_path(entry);
                (entry.name.clone(), full.to_string_lossy().to_string())
            })
            .collect();

        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let folder = match filechooser::pick(Pick::Folder, "Save to the computer")
                .await
                .ok()
                .flatten()
                .and_then(|mut dirs| dirs.drain(..).next())
            {
                Some(folder) => folder,
                None => return,
            };

            me.update(cx, |this, cx| {
                for (name, source) in sources {
                    let is_dir = this
                        .browser
                        .read(cx)
                        .entry_named(&name)
                        .is_some_and(|entry| entry.looks_like_dir());
                    let local = folder.join(&name);
                    if is_dir {
                        let policy = daemon::transfer_policy(cx);
                        let device = device.clone();
                        let me = cx.entity();
                        cx.spawn(async move |_this, cx| {
                            let result = daemon::enqueue_tree_pull(
                                &device,
                                &source,
                                &local.to_string_lossy(),
                                policy,
                            )
                            .await;
                            me.update(cx, |this, cx| this.on_tree_result("Download", result, cx))
                                .ok();
                        })
                        .detach();
                    } else {
                        this.enqueue_pull(device.clone(), PathBuf::from(source), local, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_tree_result(
        &mut self,
        action: &str,
        result: Result<crate::protocol::TreeEnqueueResult, String>,
        cx: &mut Context<Self>,
    ) {
        // A partial failure can list a dozen paths, which a toast would clip, so
        // anything other than a clean run gets a dialog.
        match result {
            Ok(outcome) => match tree_result_message(action, &outcome) {
                Ok(msg) if outcome.errors.is_empty() => self.toasts.push(msg, ToastTone::Success),
                Ok(msg) => {
                    self.dialog = Dialog::Message {
                        title: format!("{action} partly finished"),
                        body: msg,
                        tone: MessageTone::Warning,
                        extra: None,
                    }
                }
                Err(msg) => {
                    self.dialog = Dialog::Message {
                        title: format!("{action} failed"),
                        body: msg,
                        tone: MessageTone::Error,
                        extra: None,
                    }
                }
            },
            Err(err) => {
                self.dialog = Dialog::Message {
                    title: format!("{action} failed"),
                    body: err,
                    tone: MessageTone::Error,
                    extra: None,
                }
            }
        }
        cx.notify();
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        let active: Vec<(u64, String)> = self
            .jobs
            .iter()
            .filter(|job| job.is_active())
            .map(|job| (job.id, job.state.clone()))
            .collect();
        if active.is_empty() {
            self.toasts.push("No active transfers", ToastTone::Info);
            cx.notify();
            return;
        }
        // If anything is still running, pause; if everything is already paused,
        // resume.
        let resume = active.iter().all(|(_, state)| state == "Paused");
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            for (id, _) in active {
                if resume {
                    let _ = daemon::resume_job(id).await;
                } else {
                    let _ = daemon::pause_job(id).await;
                }
            }
            me.update(cx, |_this, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn cancel_transfers(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self
            .jobs
            .iter()
            .filter(|job| job.is_active())
            .map(|job| job.id)
            .collect();
        if ids.is_empty() {
            cx.notify();
            return;
        }
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            for id in ids {
                let _ = daemon::cancel_job(id).await;
            }
            me.update(cx, |this, cx| {
                this.toasts.push("Transfers cancelled", ToastTone::Info);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn retry_failed(&mut self, cx: &mut Context<Self>) {
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::retry_failed().await;
            me.update(cx, |this, _cx| match result {
                Ok(count) => this
                    .toasts
                    .push(format!("Re-queued {count} transfer(s)"), ToastTone::Success),
                Err(err) => this
                    .toasts
                    .push(format!("Could not retry: {err}"), ToastTone::Error),
            })
            .ok();
        })
        .detach();
    }

    // ── Local and device mutations ─────────────────────────────────────────

    fn trash_entries(&mut self, entries: Vec<DirEntry>, cx: &mut Context<Self>) {
        let base = self.browser.read(cx).path().to_path_buf();
        let mut moved = 0;
        let mut failures: Vec<String> = Vec::new();
        for entry in &entries {
            match localfs::trash(&base.join(&entry.name)) {
                Ok(_) => moved += 1,
                Err(err) => failures.push(err),
            }
        }
        if moved > 0 {
            self.toasts.push(
                format!("Moved {moved} item(s) to Trash"),
                ToastTone::Success,
            );
        }
        if !failures.is_empty() {
            self.toasts.push(
                format!(
                    "Could not move {} item(s) to Trash: {}",
                    failures.len(),
                    failures.join("; ")
                ),
                ToastTone::Error,
            );
        }
        self.refresh_current(cx);
    }

    fn create_folder(&mut self, name: String, cx: &mut Context<Self>) {
        let name = match validate_name(&name) {
            Ok(clean) => clean,
            Err(reason) => {
                self.toasts.push(reason, ToastTone::Error);
                cx.notify();
                return;
            }
        };
        let Some(device) = self.target_device() else {
            // No device: this is the local disk, so do it here.
            let base = self.browser.read(cx).path().join(&name);
            match localfs::mkdir(&base) {
                Ok(()) => self
                    .toasts
                    .push(format!("Created {name}"), ToastTone::Success),
                Err(err) => self
                    .toasts
                    .push(format!("Could not create {name}: {err}"), ToastTone::Error),
            }
            self.refresh_current(cx);
            return;
        };

        let path = self
            .browser
            .read(cx)
            .path()
            .join(&name)
            .to_string_lossy()
            .to_string();
        let me = cx.entity();
        let shown = name.clone();
        cx.spawn(async move |_this, cx| {
            let result = daemon::mkdir(&device, &path).await;
            me.update(cx, |this, _cx| match result {
                Ok(()) => this
                    .toasts
                    .push(format!("Created {shown}"), ToastTone::Success),
                Err(err) => this
                    .toasts
                    .push(format!("Could not create {shown}: {err}"), ToastTone::Error),
            })
            .ok();
        })
        .detach();
    }

    fn rename_entry(&mut self, entry: DirEntry, name: String, cx: &mut Context<Self>) {
        let name = match validate_name(&name) {
            Ok(clean) => clean,
            Err(reason) => {
                self.toasts.push(reason, ToastTone::Error);
                cx.notify();
                return;
            }
        };
        let from = self.browser.read(cx).full_path(&entry);
        let to = from
            .parent()
            .map(|base| base.join(&name))
            .unwrap_or_else(|| PathBuf::from(&name));

        match self.target_device() {
            Some(device) => {
                let device = device.clone();
                let from = from.to_string_lossy().to_string();
                let to = to.to_string_lossy().to_string();
                let me = cx.entity();
                let shown = name.clone();
                cx.spawn(async move |_this, cx| {
                    let result = daemon::rename(&device, &from, &to).await;
                    me.update(cx, |this, _cx| match result {
                        Ok(()) => this
                            .toasts
                            .push(format!("Renamed to {shown}"), ToastTone::Success),
                        Err(err) => this
                            .toasts
                            .push(format!("Could not rename: {err}"), ToastTone::Error),
                    })
                    .ok();
                })
                .detach();
            }
            None => match localfs::rename(&from, &to) {
                Ok(()) => self
                    .toasts
                    .push(format!("Renamed to {name}"), ToastTone::Success),
                Err(err) => self
                    .toasts
                    .push(format!("Could not rename: {err}"), ToastTone::Error),
            },
        }
        self.refresh_current(cx);
    }

    fn delete_permanently(&mut self, entries: Vec<DirEntry>, cx: &mut Context<Self>) {
        let base = self.browser.read(cx).path().to_path_buf();
        let Some(device) = self.target_device() else {
            let mut failures = Vec::new();
            for entry in &entries {
                if let Err(err) = localfs::delete(&base.join(&entry.name)) {
                    failures.push(err.to_string());
                }
            }
            if failures.is_empty() {
                self.toasts.push(
                    format!("Deleted {} item(s)", entries.len()),
                    ToastTone::Success,
                );
            } else {
                self.toasts.push(failures.join("; "), ToastTone::Error);
            }
            self.refresh_current(cx);
            return;
        };

        let me = cx.entity();
        let count = entries.len();
        cx.spawn(async move |_this, cx| {
            let mut failures = Vec::new();
            for entry in &entries {
                let path = base.join(&entry.name).to_string_lossy().to_string();
                if let Err(err) = daemon::delete(&device, &path).await {
                    failures.push(err);
                }
            }
            me.update(cx, |this, cx| {
                if failures.is_empty() {
                    this.toasts.push(
                        format!("Deleted {count} item(s) from the phone"),
                        ToastTone::Success,
                    );
                } else {
                    this.toasts.push(failures.join("; "), ToastTone::Error);
                }
                this.refresh_current(cx);
            })
            .ok();
        })
        .detach();
    }

    // ── APKs ───────────────────────────────────────────────────────────────

    fn install_apk(&mut self, entry: DirEntry, cx: &mut Context<Self>) {
        // An APK already on the device is installed from there; one on the
        // computer has to be picked first.
        if self.browser.read(cx).is_local() {
            let local = self.browser.read(cx).full_path(&entry);
            self.dialog = Dialog::SideloadApk {
                name: entry.name.clone(),
                local,
            };
            cx.notify();
            return;
        }
        let Some(device) = self.target_device() else {
            return;
        };
        let path = self
            .browser
            .read(cx)
            .full_path(&entry)
            .to_string_lossy()
            .to_string();
        self.run_install(device, path, cx);
    }

    fn run_install(&mut self, device: String, path: String, cx: &mut Context<Self>) {
        let me = cx.entity();
        let label = PathBuf::from(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        cx.spawn(async move |_this, cx| {
            let result = daemon::install_apk(&device, &path).await;
            me.update(cx, |this, cx| {
                // Installing can take a while and its result matters, so it gets
                // a dialog rather than a toast the user may have missed.
                this.dialog = match result {
                    Ok(ok) if ok.trim().is_empty() || ok == "ok" => Dialog::Message {
                        title: "Installed".into(),
                        body: format!("{label} is on the phone."),
                        tone: MessageTone::Success,
                        extra: None,
                    },
                    Ok(ok) => Dialog::Message {
                        title: "Installed".into(),
                        body: format!("{label}: {ok}"),
                        tone: MessageTone::Success,
                        extra: None,
                    },
                    Err(err) => Dialog::Message {
                        title: "Could not install".into(),
                        body: format!("{label}: {err}"),
                        tone: MessageTone::Error,
                        extra: None,
                    },
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Save the focused entry to a chosen location, as "Save as" does.
    fn save_as(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.browser.read(cx).focused_entry() else {
            return;
        };
        let Some(source) = self.browser.read(cx).local_path_of(&entry) else {
            self.toasts
                .push("Saving a phone file needs a FUSE mount", ToastTone::Warning);
            cx.notify();
            return;
        };
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let picked = filechooser::save("Save as", &entry.name).await;
            let Ok(Some(targets)) = picked else { return };
            let Some(target) = targets.into_iter().next() else {
                return;
            };
            me.update(cx, |this, _cx| match localfs::copy_file(&source, &target) {
                Ok(()) => this
                    .toasts
                    .push(format!("Saved to {}", target.display()), ToastTone::Success),
                Err(err) => this
                    .toasts
                    .push(format!("Could not save: {err}"), ToastTone::Error),
            })
            .ok();
        })
        .detach();
    }

    fn copy_apk_to_device(&mut self, local: PathBuf, cx: &mut Context<Self>) {
        let Some(device) = self.target_device() else {
            return;
        };
        let destination = self
            .browser
            .read(cx)
            .path()
            .join(file_name_of(&local))
            .to_string_lossy()
            .to_string();
        self.enqueue_push(device, local, PathBuf::from(destination), cx);
    }

    // ── Dialogs ────────────────────────────────────────────────────────────

    /// Move the pairing flow to `step`, keeping the two field entities.
    fn pairing_step(&mut self, step: WifiStep) {
        self.dialog = Dialog::ConnectWifi {
            step,
            address_field: self.pair_address_field.clone(),
            code_field: self.pair_code_field.clone(),
        };
    }

    /// What the shared name field is currently collecting.
    fn name_field_purpose(&self) -> Option<NamePurpose> {
        match &self.dialog {
            Dialog::NewFolder { .. } => Some(NamePurpose::CreateFolder),
            Dialog::Rename { entry, .. } => Some(NamePurpose::Rename(entry.clone())),
            _ => None,
        }
    }

    fn on_dialog_result(&mut self, result: &DialogResult, cx: &mut Context<Self>) {
        match result {
            DialogResult::Dismiss => self.dialog = Dialog::None,
            DialogResult::CreateFolder(name) => {
                self.dialog = Dialog::None;
                self.create_folder(name.clone(), cx);
            }
            DialogResult::Rename(entry, name) => {
                self.dialog = Dialog::None;
                self.rename_entry(entry.clone(), name.clone(), cx);
            }
            DialogResult::Trash(entries) => {
                self.dialog = Dialog::None;
                self.trash_entries(entries.clone(), cx);
            }
            DialogResult::DeletePermanently(entries) => {
                self.dialog = Dialog::None;
                self.delete_permanently(entries.clone(), cx);
            }
            DialogResult::CopyDiagnostics(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                self.toasts
                    .push("Report copied to the clipboard", ToastTone::Info);
            }
            DialogResult::PairAddress(address) => {
                if !dialogs::address_is_well_formed(address) {
                    self.toasts.push(
                        "Pairing address must look like 192.168.1.20:37001",
                        ToastTone::Error,
                    );
                    cx.notify();
                    return;
                }
                self.pairing_step(WifiStep::Pairing {
                    address: address.clone(),
                });
            }
            DialogResult::PairCode { address, code } => {
                if let Err(reason) = dialogs::validate_pair_input(address, code) {
                    self.toasts.push(reason, ToastTone::Error);
                    cx.notify();
                    return;
                }
                self.run_pair(address.clone(), code.clone(), cx);
            }
            DialogResult::ConnectWireless(address) => {
                self.dialog = Dialog::None;
                self.run_connect(address.clone(), cx);
            }
            DialogResult::OpenExternal(path) => {
                if let Err(err) = localfs::open_external(path) {
                    self.toasts.push(err, ToastTone::Error);
                }
            }
            DialogResult::InstallApk(path) => {
                self.dialog = Dialog::None;
                if let Some(device) = self.target_device() {
                    self.run_install(device, path.to_string_lossy().to_string(), cx);
                }
            }
            DialogResult::CopyApk(path) => {
                self.dialog = Dialog::None;
                self.copy_apk_to_device(path.clone(), cx);
            }
        }
        cx.notify();
    }

    fn run_pair(&mut self, address: String, code: String, cx: &mut Context<Self>) {
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::pair_wireless(&address, &code).await;
            me.update(cx, |this, cx| {
                this.pairing_step(match &result {
                    Ok(_) => WifiStep::Paired {
                        address: address.clone(),
                    },
                    Err(err) => WifiStep::Failed {
                        message: format!("Pairing failed: {err}"),
                    },
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_connect(&mut self, address: String, cx: &mut Context<Self>) {
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::connect_wireless(&address).await;
            me.update(cx, |this, _cx| match result {
                Ok(ok) => {
                    this.toasts.push(
                        if ok.trim().is_empty() {
                            format!("Connected to {address}")
                        } else {
                            format!("Connected to {address}: {ok}")
                        },
                        ToastTone::Success,
                    );
                }
                Err(err) => this.pairing_step(WifiStep::Failed {
                    message: format!("Could not connect to {address}: {err}"),
                }),
            })
            .ok();
        })
        .detach();
    }

    fn open_diagnostics(&mut self, cx: &mut Context<Self>) {
        let me = cx.entity();
        cx.spawn(async move |_this, cx| {
            let result = daemon::diagnostics().await;
            me.update(cx, |this, cx| {
                this.dialog = match result {
                    Ok(report) => Dialog::Diagnostics { report },
                    Err(err) => Dialog::Message {
                        title: "Diagnostics unavailable".into(),
                        body: format!("adb-daemon did not answer: {err}"),
                        tone: MessageTone::Error,
                        extra: None,
                    },
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── Toasts ─────────────────────────────────────────────────────────────

    fn on_toast_action(&mut self, id: ActionId, cx: &mut Context<Self>) {
        match id {
            ActionId::RetryFailed => self.retry_failed(cx),
            ActionId::OpenDownloads => {
                if let Err(err) = localfs::open_external(&default_save_dir()) {
                    self.toasts.push(err, ToastTone::Error);
                }
            }
            ActionId::CopyDiagnostics => self.open_diagnostics(cx),
        }
        self.toasts.dismiss_action(id);
        cx.notify();
    }

    // ── Menu plumbing ──────────────────────────────────────────────────────

    /// Run whatever a menu row asked for, then close the menu.
    fn on_menu_select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.overflow_open = false;
        // Everything below can change a persisted preference.
        match id {
            "refresh" => self.refresh_current(cx),
            "new-folder" => {
                self.name_field.update(cx, |f, cx| {
                    f.set_value("", cx);
                    f.select_everything(cx);
                });
                self.dialog = Dialog::NewFolder {
                    field: self.name_field.clone(),
                };
            }
            "upload" => self.pick_for_upload(cx),
            "upload-folder" => self.pick_folder_for_upload(cx),
            "download" => {
                let entries = self.browser.read(cx).selected();
                if entries.is_empty() {
                    self.toasts
                        .push("Select something to save first", ToastTone::Warning);
                } else {
                    self.download(entries, cx);
                }
            }
            "select-all" => self.browser.update(cx, |b, cx| {
                b.select_all_entries();
                cx.notify();
            }),
            "show-hidden" => {
                let hidden = self.browser.read(cx).show_hidden();
                self.browser
                    .update(cx, |b, cx| b.set_show_hidden(!hidden, cx));
            }
            "sort-name" | "sort-size" | "sort-modified" => {
                let key = match id {
                    "sort-size" => SortKey::Size,
                    "sort-modified" => SortKey::Modified,
                    _ => SortKey::Name,
                };
                // Changing the key keeps the current direction, so a user who
                // prefers descending order does not have to set it again.
                let descending = self.browser.read(cx).sort_descending();
                self.browser
                    .update(cx, |b, cx| b.set_sort(key, descending, cx));
            }
            "sort-reverse" => {
                let key = self.browser.read(cx).sort_key();
                let descending = !self.browser.read(cx).sort_descending();
                self.browser
                    .update(cx, |b, cx| b.set_sort(key, descending, cx));
            }
            "open" | "open-folder" => {
                if let Some(entry) = self.browser.read(cx).focused_entry() {
                    let full = self.browser.read(cx).full_path(&entry);
                    if entry.looks_like_dir() {
                        self.browser.update(cx, |b, _| b.navigate(full.clone()));
                        self.on_browser_event(&BrowserEvent::OpenedDirectory(full), cx);
                    } else {
                        self.open_entry(entry, cx);
                    }
                }
            }
            "preview" => {
                if let Some(entry) = self.browser.read(cx).focused_entry() {
                    // A device image is only readable through its FUSE mount.
                    match self.browser.read(cx).local_path_of(&entry) {
                        Some(local) => {
                            self.dialog = Dialog::ImagePreview {
                                name: entry.name.clone(),
                                local,
                            };
                        }
                        None => self.toasts.push(
                            "Previewing a phone file needs a FUSE mount",
                            ToastTone::Warning,
                        ),
                    }
                }
            }
            "save-as" => self.save_as(cx),
            "open-external" => {
                if let Some(entry) = self.browser.read(cx).focused_entry() {
                    self.open_entry(entry, cx);
                }
            }
            "install-apk" => {
                if let Some(entry) = self.browser.read(cx).focused_entry() {
                    self.install_apk(entry, cx);
                }
            }
            "open-terminal" => {
                let path = self.browser.read(cx).path().to_path_buf();
                if let Err(err) = localfs::open_terminal(&path) {
                    self.toasts.push(err, ToastTone::Error);
                }
            }
            "properties" => {
                if let Some(entry) = self.browser.read(cx).focused_entry() {
                    let full = self.browser.read(cx).full_path(&entry);
                    let device = self.target_device().unwrap_or_else(|| LOCAL_DEVICE.into());
                    self.dialog = Dialog::Properties {
                        entry,
                        full_path: full,
                        device,
                    };
                }
            }
            "copy" => {
                let entries = self.browser.read(cx).selected();
                if entries.is_empty() {
                    self.toasts.push("Nothing selected", ToastTone::Warning);
                } else {
                    self.copy(entries, cx);
                }
            }
            "paste" => self.paste(cx),
            "rename" => {
                if let Some(entry) = self.browser.read(cx).focused_entry() {
                    self.name_field.update(cx, |f, cx| {
                        f.set_value(entry.name.clone(), cx);
                        f.select_everything(cx);
                    });
                    self.dialog = Dialog::Rename {
                        entry,
                        field: self.name_field.clone(),
                    };
                }
            }
            "trash" | "delete" => {
                let entries = self.browser.read(cx).selected();
                if entries.is_empty() {
                    self.toasts.push("Nothing selected", ToastTone::Warning);
                } else {
                    self.dialog = Dialog::ConfirmDelete {
                        label: describe_selection(&entries),
                        entries,
                        trash: id == "trash",
                    };
                }
            }
            "diagnostics" => self.open_diagnostics(cx),
            "shortcuts" => self.dialog = Dialog::Shortcuts,
            "about" => self.dialog = Dialog::About,
            "connect-wifi" => {
                self.pair_address_field
                    .update(cx, |f, cx| f.select_everything(cx));
                self.pairing_step(WifiStep::Address);
            }
            "open-downloads" => {
                if let Err(err) = localfs::open_external(&default_save_dir()) {
                    self.toasts.push(err, ToastTone::Error);
                }
            }
            "retry-failed" => self.retry_failed(cx),
            "toggle-sidebar" => {
                self.sidebar_visible = !self.sidebar_visible;
            }
            _ => {}
        }
        if matches!(
            id,
            "show-hidden"
                | "toggle-sidebar"
                | "sort-name"
                | "sort-size"
                | "sort-modified"
                | "sort-reverse"
        ) {
            self.persist_if_changed(cx);
        }
        cx.notify();
    }

    /// The overflow menu, for the current state.
    fn overflow_items(&self, show_hidden: bool, sort: (SortKey, bool)) -> Vec<MenuItem> {
        // Generated from the enum, so a new sort key cannot be added to the
        // model and then forgotten here.
        let sort_rows: Vec<MenuItem> = SortKey::values()
            .map(|key| MenuItem::Check {
                id: key.id(),
                icon: names::OBJECT_SELECT,
                label: key.label().to_string(),
                checked: sort.0 == key,
            })
            .chain(std::iter::once(MenuItem::Check {
                id: "sort-reverse",
                icon: names::EDIT_UNDO,
                label: "Reverse order".to_string(),
                checked: sort.1,
            }))
            .collect();

        let mut items = vec![
            MenuItem::with_icon("refresh", names::REFRESH, "Refresh", Some("F5")),
            MenuItem::with_icon(
                "new-folder",
                names::FOLDER_NEW,
                "New folder",
                Some("Ctrl+N"),
            ),
            MenuItem::action("select-all", "Select all").shortcut("Ctrl+A"),
            MenuItem::Separator,
            MenuItem::with_icon(
                "upload",
                names::FOLDER_UPLOAD,
                "Send to phone…",
                Some("Ctrl+U"),
            ),
            MenuItem::with_icon(
                "upload-folder",
                names::FOLDER_SEND,
                "Send a folder to phone…",
                None,
            ),
            MenuItem::with_icon(
                "download",
                names::FOLDER_DOWNLOAD,
                "Save to computer…",
                Some("Ctrl+Shift+C"),
            ),
            MenuItem::Separator,
            MenuItem::with_icon("open-external", names::FILE_MANAGER, "Open in Files", None),
            MenuItem::with_icon(
                "open-terminal",
                names::TERMINAL,
                "Open in terminal",
                Some("Alt+T"),
            ),
            MenuItem::with_icon(
                "diagnostics",
                names::DIALOG_INFORMATION,
                "Connection diagnostics…",
                None,
            ),
            MenuItem::Heading("Sort by".into()),
            MenuItem::Check {
                id: "show-hidden",
                icon: names::OBJECT_SELECT,
                label: "Show hidden files".into(),
                checked: show_hidden,
            },
            MenuItem::Separator,
            MenuItem::with_icon(
                "shortcuts",
                names::DIALOG_INFORMATION,
                "Keyboard shortcuts",
                None,
            ),
            MenuItem::with_icon("about", names::HELP_ABOUT, "About ADBShare", None),
        ];

        // Splice the generated sort rows in right after their heading.
        let at = items
            .iter()
            .position(|item| matches!(item, MenuItem::Heading(h) if h == "Sort by"))
            .map(|index| index + 1)
            .unwrap_or(items.len());
        items.splice(at..at, sort_rows);
        items
    }

    /// The right-click menu for the row under the pointer.
    fn context_items(&self, browser: &Browser) -> Vec<MenuItem> {
        let selected = browser.selected();
        let single = selected.len() == 1;
        let focused = browser.focused_entry();
        let local = browser.is_local();
        let has_apk = selected.iter().any(|e| e.ext() == "apk");

        let has_selection = !selected.is_empty();
        let mut items = Vec::new();
        if single && let Some(entry) = &focused {
            if entry.looks_like_dir() {
                items.push(MenuItem::with_icon(
                    "open-folder",
                    names::FOLDER_OPEN,
                    "Open",
                    Some("Return"),
                ));
            } else {
                items.push(MenuItem::with_icon(
                    "open",
                    names::DOCUMENT_OPEN,
                    "Open",
                    Some("Return"),
                ));
            }
        }
        items.push(MenuItem::Separator);
        items.push(if has_selection {
            MenuItem::with_icon(
                "download",
                names::FOLDER_DOWNLOAD,
                if local {
                    "Copy to Downloads"
                } else {
                    "Save to computer"
                },
                Some("Ctrl+Shift+C"),
            )
        } else {
            MenuItem::disabled(
                "download",
                names::FOLDER_DOWNLOAD,
                "Save to computer",
                Some("Ctrl+Shift+C"),
            )
        });
        items.push(MenuItem::with_icon(
            "upload",
            names::SEND_TO,
            "Send files to phone…",
            Some("Ctrl+U"),
        ));
        if has_apk {
            items.push(MenuItem::with_icon(
                "install-apk",
                names::SOFTWARE_INSTALL,
                "Install APK on phone",
                None,
            ));
        }
        items.push(MenuItem::Separator);
        items.push(if has_selection {
            MenuItem::with_icon("copy", names::EDIT_COPY, "Copy", Some("Ctrl+C"))
        } else {
            MenuItem::disabled("copy", names::EDIT_COPY, "Copy", Some("Ctrl+C"))
        });
        items.push(MenuItem::with_icon(
            "paste",
            names::EDIT_PASTE,
            "Paste",
            Some("Ctrl+V"),
        ));
        if single {
            items.push(MenuItem::with_icon(
                "rename",
                names::DOCUMENT_EDIT,
                "Rename…",
                Some("F2"),
            ));
        }
        if single && !local {
            items.push(MenuItem::with_icon(
                "save-as",
                names::EDIT_PASTE,
                "Save as\u{2026}",
                None,
            ));
        }
        if single {
            let is_image = focused.as_ref().is_some_and(|e| {
                matches!(e.ext().as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")
            });
            if is_image {
                items.push(MenuItem::with_icon(
                    "preview",
                    names::IMAGE_GENERIC,
                    "Preview",
                    None,
                ));
            }
        }
        if local {
            items.push(MenuItem::with_icon(
                "trash",
                names::TRASH,
                "Move to Trash",
                Some("Delete"),
            ));
        }
        items.push(MenuItem::danger(
            "delete",
            names::EDIT_DELETE,
            if local {
                "Delete permanently".to_string()
            } else {
                "Delete permanently from phone".to_string()
            },
        ));
        items.push(MenuItem::Heading(if local {
            "On this computer".to_string()
        } else {
            "On the phone".to_string()
        }));
        if single {
            items.push(MenuItem::with_icon(
                "open-terminal",
                names::TERMINAL,
                "Open in terminal",
                Some("Alt+T"),
            ));
            items.push(MenuItem::with_icon(
                "properties",
                names::DIALOG_INFORMATION,
                "Properties",
                Some("Alt+Return"),
            ));
        }
        items
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

/// The window's own controls. The GTK build drew a custom close button because
/// the title bar was hidden; GPUI gives us a real title bar, so the window keeps
/// the platform's own decoration and the top bar starts after it.
impl AdbShareApp {
    /// The top bar: navigation, the path bar, and the action capsules.
    fn topbar(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut capsules: Vec<AnyElement> = Vec::new();

        // Navigation.
        let browser = self.browser.clone();
        #[allow(unused_mut)]
        let mut browser = browser;
        capsules.push(
            ui::capsule([
                ui::icon_button(
                    t,
                    "toggle-sidebar",
                    names::SIDEBAR_SHOW,
                    theme::CAPSULE_BTN,
                    t.text_dim,
                    cx.listener(|this, _e, _w, cx| {
                        this.sidebar_visible = !this.sidebar_visible;
                        cx.notify();
                    }),
                )
                .into_any_element(),
                ui::capsule_separator(t).into_any_element(),
                self.nav_button(t, "back", names::GO_PREVIOUS, &browser, cx),
                self.nav_button(t, "forward", names::GO_NEXT, &browser, cx),
                self.nav_button(t, "up", names::GO_UP, &browser, cx),
                ui::capsule_separator(t).into_any_element(),
                self.nav_button(t, "refresh", names::REFRESH, &browser, cx),
            ])
            .into_any_element(),
        );

        // The path bar, which doubles as the search box and the Ctrl+L path
        // editor.
        capsules.push(self.omnibar(t, cx));

        if self.layout.shows_ops() {
            let _browser = self.browser.clone();
            capsules.push(
                ui::capsule([
                    ui::icon_button(
                        t,
                        "new-folder",
                        names::FOLDER_NEW,
                        theme::CAPSULE_BTN,
                        t.text_dim,
                        cx.listener(|this, _e, _w, cx| {
                            this.on_menu_select("new-folder", cx);
                        }),
                    )
                    .into_any_element(),
                    ui::capsule_separator(t).into_any_element(),
                    ui::icon_button(
                        t,
                        "upload",
                        names::FOLDER_UPLOAD,
                        theme::CAPSULE_BTN,
                        t.text_dim,
                        cx.listener(|this, _e, _w, cx| this.on_menu_select("upload", cx)),
                    )
                    .into_any_element(),
                    ui::icon_button(
                        t,
                        "download",
                        names::FOLDER_DOWNLOAD,
                        theme::CAPSULE_BTN,
                        t.text_dim,
                        cx.listener(|this, _e, _w, cx| this.on_menu_select("download", cx)),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );

            // Grid/list switch plus search, the way the old view capsule did.
            let grid = self.browser.read(cx).view_mode() == ViewMode::Grid;
            let search_on = self.browser.read(cx).search_active();
            capsules.push(
                ui::capsule([
                    ui::icon_button_active(
                        t,
                        "view-grid",
                        names::VIEW_GRID,
                        theme::CAPSULE_BTN,
                        grid,
                        cx.listener(|_this, _e, _w, cx| {
                            cx.dispatch_action(&crate::browser::ToggleView);
                        }),
                    )
                    .into_any_element(),
                    ui::icon_button_active(
                        t,
                        "view-list",
                        names::VIEW_LIST,
                        theme::CAPSULE_BTN,
                        !grid,
                        cx.listener(|_this, _e, _w, cx| {
                            cx.dispatch_action(&crate::browser::ToggleView);
                        }),
                    )
                    .into_any_element(),
                    ui::capsule_separator(t).into_any_element(),
                    ui::icon_button_active(
                        t,
                        "search",
                        names::EDIT_FIND,
                        theme::CAPSULE_BTN,
                        search_on,
                        cx.listener(|_this, _e, _w, cx| {
                            cx.dispatch_action(&crate::browser::ToggleSearch);
                        }),
                    )
                    .into_any_element(),
                ])
                .into_any_element(),
            );
        }

        capsules.push(self.transfers_button(t, cx));
        capsules.push(self.overflow_button(t, cx));

        div()
            .relative()
            .flex()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .h(px(theme::TOPBAR_H))
            .px(px(8.0))
            .bg(t.topbar)
            .border_b_1()
            .border_color(t.border)
            .children(capsules)
            .child(self.transfers_popover(t, cx))
            .child(self.overflow_popover(t, cx))
            .into_any_element()
    }

    /// A navigation button that is dimmed when there is nowhere to go.
    fn nav_button(
        &self,
        t: &theme::Palette,
        id: &'static str,
        icon: &'static str,
        browser: &Entity<Browser>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let enabled = match id {
            "back" => browser.read(cx).can_go_back(),
            "forward" => browser.read(cx).can_go_forward(),
            "up" => browser.read(cx).can_go_up(),
            _ => true,
        };
        if !enabled {
            return ui::icon_button_disabled(t, icon, theme::CAPSULE_BTN).into_any_element();
        }
        ui::icon_button(
            t,
            id,
            icon,
            theme::CAPSULE_BTN,
            t.text_dim,
            move |_, window, cx| {
                // Navigation is a browser action, so dispatch it rather than
                // reimplementing the history here.
                let action: Box<dyn gpui::Action> = match id {
                    "back" => Box::new(crate::browser::Back),
                    "forward" => Box::new(crate::browser::Forward),
                    "up" => Box::new(crate::browser::Up),
                    _ => Box::new(crate::browser::Refresh),
                };
                cx.dispatch_action(action.as_ref());
                let _ = window;
            },
        )
        .into_any_element()
    }

    /// The path bar: breadcrumbs, or the typed path when Ctrl+L is active.
    fn omnibar(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        let _browser = self.browser.clone();
        if self.browser.read(cx).path_entry_active() {
            // Seed the field with the current path so Enter works unchanged.
            let current = self.browser.read(cx).path().to_string_lossy().to_string();
            if self.path_field.read(cx).value() != current {
                self.path_field.update(cx, |f, cx| f.set_value(current, cx));
            }
            let field = self.path_field.clone();
            return div()
                .flex()
                .items_center()
                .h(px(theme::CAPSULE_H))
                .px(px(6.0))
                .rounded(px(theme::RADIUS_CAPSULE))
                .bg(t.capsule_bg)
                .border_1()
                .border_color(t.capsule_border)
                .w(px(584.0))
                .child(field)
                .into_any_element();
        }

        let crumbs = self.browser.read(cx).breadcrumbs();
        let crumb_owner = cx.entity();
        let current = crumbs.last().map(|(_, path)| path.clone());
        let pills: Vec<AnyElement> = crumbs
            .iter()
            .enumerate()
            .map(|(index, (label, path))| {
                let is_current = Some(path) == current.as_ref();
                let target = path.clone();
                let me = crumb_owner.clone();
                let separator = (index > 0).then(|| ui::breadcrumb_separator(t).into_any_element());
                let pill = div()
                    .id(("crumb", index))
                    .flex()
                    .items_center()
                    .h(px(26.0))
                    .px(px(8.0))
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .text_color(if is_current {
                        t.text_header
                    } else {
                        t.text_dim
                    })
                    .font_weight(if is_current {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .hover(|s| s.bg(t.hover).text_color(t.text_header))
                    .child(label.clone())
                    .on_click(move |_, _w, cx| {
                        // Navigate carries both the move and the fetch, so a
                        // crumb click and a typed path behave identically.
                        let target = target.clone();
                        me.update(cx, |app, cx| {
                            app.on_browser_event(&BrowserEvent::Navigate(target), cx);
                        });
                    });
                let mut row = div().flex().items_center();
                if let Some(sep) = separator {
                    row = row.child(sep);
                }
                row.child(pill).into_any_element()
            })
            .collect();

        div()
            .flex()
            .items_center()
            .h(px(theme::CAPSULE_H))
            .flex_1()
            .min_w_0()
            .px(px(8.0))
            .rounded(px(theme::RADIUS_CAPSULE))
            // `toolbar.background`: the darker step Zed uses for its strips,
            // which is what a path bar is.
            .bg(t.topbar_raised)
            .border_1()
            .border_color(t.border_soft)
            .id("crumbs")
            .overflow_x_scroll()
            .scrollbar_width(px(0.0))
            .child(icons::icon(
                if self.browser.read(cx).is_local() {
                    names::DRIVE_HARDDISK
                } else {
                    names::PHONE
                },
                14.0,
                t.text_dim,
            ))
            .child(
                div()
                    .mx(px(6.0))
                    .child(div().flex().items_center().children(pills)),
            )
            .into_any_element()
    }

    /// The transfers button, with an activity dot when work is in flight.
    fn transfers_button(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        let active = self.jobs.iter().filter(|job| job.is_active()).count();
        div()
            .id("transfers")
            .flex()
            .items_center()
            .h(px(theme::CAPSULE_H))
            .px(px(12.0))
            .rounded(px(theme::RADIUS_CAPSULE))
            .cursor_pointer()
            .text_sm()
            .text_color(t.text_dim)
            .hover(|s| s.bg(t.hover).text_color(rgba(0xFFFFFFFF)))
            .child(icons::icon(names::EMBLEM_SYNC, 15.0, t.text_dim))
            .child("Transfers")
            .when(active > 0, |d| {
                d.child(div().mx(px(6.0)).child(ui::led(t.success)))
            })
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.transfers_open = !this.transfers_open;
                this.overflow_open = false;
                cx.notify();
            }))
            .into_any_element()
    }

    /// The overflow menu button.
    fn overflow_button(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("overflow")
            .flex()
            .items_center()
            .justify_center()
            .size(px(theme::CAPSULE_H))
            .rounded(px(theme::RADIUS_CAPSULE))
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .child(icons::icon(names::VIEW_MORE, 15.0, t.text_dim))
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.overflow_open = !this.overflow_open;
                this.transfers_open = false;
                cx.notify();
            }))
            .into_any_element()
    }

    /// The transfers popover, anchored under its button.
    fn transfers_popover(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.transfers_open {
            return div().into_any_element();
        }
        let policy = daemon::transfer_policy(cx);
        let jobs = self.jobs.clone();

        let body: Vec<AnyElement> = if jobs.is_empty() {
            vec![
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(18.0))
                    .py(px(28.0))
                    .child(icons::icon(names::EMBLEM_SYNC, 28.0, t.text_muted))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_secondary)
                            .child("No transfers yet"),
                    )
                    .child(
                        div()
                            .max_w(px(240.0))
                            .text_size(px(11.0))
                            .text_color(t.text_muted)
                            .whitespace_normal()
                            .child(
                                "Send files to the phone or save them to this computer and \
                                 they will show up here.",
                            ),
                    )
                    .into_any_element(),
            ]
        } else {
            jobs.iter()
                .map(|job| render_job_card(t, job))
                .collect::<Vec<_>>()
        };

        let (overwrite, verify) = policy;
        let owner = cx.entity();
        let rows = ui::surface(t, body)
            .w(px(340.0))
            .max_h(px(420.0))
            .id("transfer-list")
            .overflow_y_scroll()
            .scrollbar_width(px(6.0));

        ui::on_top(
            div()
                .absolute()
                .top(px(theme::TOPBAR_H - 2.0))
                .right(px(44.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .w(px(340.0))
                        .rounded(px(12.0))
                        .bg(t.card)
                        .border_1()
                        .border_color(t.border)
                        .shadow_xl()
                        .overflow_hidden()
                        .child(rows)
                        .child(conflict_policy_row(t, &overwrite, verify, &owner))
                        .child(retry_row(t, &owner)),
                ),
        )
        .into_any_element()
    }

    /// The overflow menu popover.
    fn overflow_popover(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.overflow_open {
            return div().into_any_element();
        }
        let show_hidden = self.browser.read(cx).show_hidden();
        let sort = (
            self.browser.read(cx).sort_key(),
            self.browser.read(cx).sort_descending(),
        );
        let items = self.overflow_items(show_hidden, sort);
        let me = cx.entity();
        let card = menu::render(t, &items, 260.0, move |id, _window, app| {
            me.update(app, |this, cx| this.on_menu_select(id, cx));
        });

        ui::on_top(
            div()
                .absolute()
                .top(px(theme::TOPBAR_H - 2.0))
                .right(px(8.0))
                .child(card),
        )
        .into_any_element()
    }

    /// The right-click menu, placed at the pointer.
    fn context_popover(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = self.context_menu.clone() else {
            return div().into_any_element();
        };
        let browser = self.browser.read(cx);
        let items = self.context_items(browser);
        let me = cx.entity();
        let card = menu::render(t, &items, 230.0, move |id, _window, app| {
            me.update(app, |this, cx| this.on_menu_select(id, cx));
        });

        ui::on_top(
            gpui::anchored()
                .position(state.position)
                .position_mode(gpui::AnchoredPositionMode::Window)
                .child(card),
        )
        .into_any_element()
    }

    /// The drag handle on the sidebar's right edge.
    ///
    /// Pressing it starts a resize; the gesture itself is handled by
    /// [`Self::resize_capture`], which covers the window while the button is
    /// held so the pointer can leave the 4px handle.
    fn sidebar_divider(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-divider")
            .w(px(4.0))
            .h_full()
            .flex_shrink_0()
            .cursor_col_resize()
            .hover(|s| s.bg(t.capsule_border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.resizing_sidebar = true;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// A full-window capture shown while the sidebar is being dragged.
    fn resize_capture(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("sidebar-resize")
            .absolute()
            .inset_0()
            .cursor_col_resize()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let width = f32::from(event.position.x);
                this.sidebar_width = width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
                window.refresh();
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.resizing_sidebar = false;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// The sidebar: devices, then phone places, then local places.
    fn sidebar(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        let has_device = self.target_device().is_some();
        let selected = self.selection.clone();
        let mut rows: Vec<AnyElement> = Vec::new();

        rows.push(ui::section_heading("Phones & tablets", t).into_any_element());
        if let Some(error) = self.daemon_error.clone() {
            // An unreachable daemon and an unplugged phone look identical from
            // the device list alone, so say which one it is.
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .px(px(10.0))
                    .py(px(9.0))
                    .rounded(px(10.0))
                    .bg(t.danger_soft)
                    .border_1()
                    .border_color(t.danger_border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.danger)
                            .child(icons::icon(names::DIALOG_WARNING, 14.0, t.danger))
                            .child("Cannot reach adb-daemon"),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(t.text_dim)
                            .whitespace_normal()
                            .child(error),
                    )
                    .child(
                        div()
                            .id("sidebar-diagnose")
                            .mt(px(4.0))
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .text_size(px(11.0))
                            .text_color(t.text_header)
                            .cursor_pointer()
                            .hover(|s| s.text_color(t.danger))
                            .child(icons::icon(names::DIALOG_INFORMATION, 12.0, t.text_dim))
                            .child("Run connection diagnostics")
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.open_diagnostics(cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        if self.devices.is_empty() && self.daemon_error.is_none() {
            rows.push(onboarding_card(t).into_any_element());
        } else if !self.devices.is_empty() {
            for device in self.devices.clone() {
                let active = matches!(&selected, Some(Selection::Device(s)) if *s == device.serial);
                let serial = device.serial.clone();
                rows.push(
                    device_card(
                        t,
                        device,
                        active,
                        cx.listener(move |this, _e, _w, cx| {
                            this.select_device(&serial, cx);
                        }),
                    )
                    .into_any_element(),
                );
            }
        }

        rows.push(
            div()
                .id("connect-wifi")
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .h(px(32.0))
                .my(px(4.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .text_size(px(12.0))
                .text_color(t.text_dim)
                .hover(|s| s.bg(t.hover).text_color(t.accent))
                .child(icons::icon(names::WIRELESS, 15.0, t.text_muted))
                .child("Connect via Wi-Fi")
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.on_menu_select("connect-wifi", cx);
                }))
                .into_any_element(),
        );

        rows.push(ui::section_heading("Phone folders", t).into_any_element());
        if !has_device {
            rows.push(
                div()
                    .px(px(4.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .text_color(t.text_muted)
                    .whitespace_normal()
                    .child("Select a phone above to browse its folders.")
                    .into_any_element(),
            );
        } else {
            for (label, icon, path) in PHONE_PLACES {
                let path = PathBuf::from(*path);
                let active = self.browser.read(cx).path() == path;
                rows.push(
                    place_row(
                        t,
                        icon,
                        label,
                        active,
                        cx.listener(move |this, _e, _w, cx| {
                            this.on_browser_event(&BrowserEvent::Navigate(path.clone()), cx);
                        }),
                    )
                    .into_any_element(),
                );
            }
        }

        rows.push(ui::section_heading("This computer", t).into_any_element());
        for (label, icon, kind) in [
            ("Home", names::HOME, LocalPlace::Home),
            ("Downloads", names::FOLDER_DOWNLOAD, LocalPlace::Downloads),
            ("Trash", names::TRASH, LocalPlace::Trash),
        ] {
            rows.push(
                place_row(
                    t,
                    icon,
                    label,
                    false,
                    cx.listener(move |this, _e, _w, cx| {
                        if let Some(path) = kind.resolve() {
                            this.select_local(&path, cx);
                        }
                    }),
                )
                .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .w(px(self.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .px(px(6.0))
            .py(px(6.0))
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.border_soft)
            .child(ui::scroll_area(rows))
            .into_any_element()
    }
}

/// A local place from the sidebar.
#[derive(Debug, Clone, Copy)]
enum LocalPlace {
    Home,
    Downloads,
    Trash,
}

impl LocalPlace {
    fn resolve(self) -> Option<PathBuf> {
        match self {
            LocalPlace::Home => dirs::home_dir(),
            LocalPlace::Downloads => dirs::download_dir().or_else(dirs::home_dir),
            LocalPlace::Trash => dirs::data_dir().map(|d| d.join("Trash/files")),
        }
    }
}

/// What to do when nothing is connected.
fn onboarding_card(t: &theme::Palette) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .px(px(10.0))
        .py(px(10.0))
        .rounded(px(10.0))
        // One Dark's panel is darker than its background, so a card on the
        // sidebar has to use the *lighter* background token to read as raised.
        .bg(t.canvas)
        .border_1()
        .border_color(t.border_soft)
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.text_header)
                .child("No phone connected"),
        )
        .children(
            [
                "Connect the phone with a USB cable.",
                "On the phone: allow USB debugging, then tap Allow.",
                "Or use Connect via Wi-Fi below.",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                div()
                    .text_size(px(11.0))
                    .text_color(t.text_dim)
                    .whitespace_normal()
                    .child(format!("{}. {step}", index + 1))
            })
            .collect::<Vec<_>>(),
        )
}

/// A device card: name, transport, battery, and a storage bar.
fn device_card(
    t: &theme::Palette,
    device: DeviceEntry,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let mut head: Vec<AnyElement> = vec![
        icons::icon(
            names::PHONE,
            16.0,
            if active { t.text_header } else { t.text_muted },
        )
        .into_any_element(),
    ];
    head.push(
        div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(if active {
                t.text_header
            } else {
                t.text_primary
            })
            .child(device.display_name().to_string())
            .into_any_element(),
    );
    head.push(
        ui::pill(
            device.transport_label(),
            if active { t.text_inverse } else { t.text_dim },
            if active {
                t.text_header
            } else {
                rgba(0xFFFFFF14)
            },
        )
        .into_any_element(),
    );
    if let Some(pct) = device.battery_pct {
        let low = pct <= 20;
        head.push(
            ui::pill(
                format!("{pct}%"),
                if low { t.danger } else { t.text_dim },
                rgba(0xFFFFFF14),
            )
            .into_any_element(),
        );
    }

    let mut card = div()
        .id(ui::el_id(format!("device:{}", device.serial.clone())))
        .flex()
        .flex_col()
        .gap(px(5.0))
        .w_full()
        .px(px(10.0))
        .py(px(9.0))
        .rounded(px(theme::RADIUS_CARD))
        .cursor_pointer()
        // A device card sits on the sidebar, so it takes the lighter surface to
        // read as raised, and the focused border when it is the current device.
        .bg(if active { t.selected } else { t.canvas })
        .border_1()
        .border_color(if active {
            t.accent_muted
        } else {
            t.border_soft
        })
        .child(div().flex().items_center().gap(px(7.0)).children(head));

    if let Some((used, total)) = device.storage.filter(|(_, total)| *total > 0) {
        let fraction = ((used as f64) / (total as f64)).clamp(0.0, 1.0) as f32;
        card = card
            .child(div().mt(px(3.0)).child(ui::progress_bar(t, fraction, 3.0)))
            .child(
                div()
                    .mt(px(1.0))
                    .text_size(px(10.5))
                    .text_color(t.text_muted)
                    .child(format_capacity((used, total))),
            );
    }

    card.on_click(on_click)
}

/// An icon + label row in the sidebar.
fn place_row(
    t: &theme::Palette,
    icon: &str,
    label: &str,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(ui::el_id(format!("place:{label}")))
        .flex()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .px(px(10.0))
        .py(px(6.0))
        .my(px(1.0))
        .rounded(px(theme::RADIUS_ROW))
        .cursor_pointer()
        .text_size(px(12.5))
        .text_color(if active {
            t.text_header
        } else {
            t.text_secondary
        })
        .border_1()
        .border_color(if active {
            rgba(0xFFFFFF14)
        } else {
            rgba(0x00000000)
        })
        .bg(if active { t.hover } else { rgba(0x00000000) })
        .hover(|s| s.bg(t.hover).text_color(t.text_header))
        .child(icons::icon(
            icon,
            16.0,
            if active { t.text_header } else { t.text_muted },
        ))
        .child(div().flex_1().min_w_0().truncate().child(label.to_string()))
        .on_click(on_click)
}

/// One transfer row in the popover.
fn render_job_card(t: &theme::Palette, job: &JobInfo) -> AnyElement {
    let (state_text, tone) = job.state_pill();
    let (fg, bg) = ui::tone_colors(t, tone);
    let (arrow, caption) = job.direction_badge();

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .px(px(12.0))
        .py(px(10.0))
        .border_b_1()
        .border_color(t.border_soft)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(t.text_header)
                        .child(arrow),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .text_color(t.text_primary)
                        .child(job.name.clone()),
                )
                .child(ui::pill(state_text, fg, bg)),
        )
        .child(ui::progress_bar(t, job.fraction(), 4.0))
        .child(
            div()
                .font_family(theme::MONO)
                .text_size(px(9.5))
                .text_color(t.text_muted)
                .truncate()
                .child(job.status_text()),
        )
        .child(
            div()
                .text_size(px(9.5))
                .text_color(t.text_muted)
                .child(caption.to_string()),
        )
        .into_any_element()
}

/// The human label for a conflict policy value.
fn policy_label(value: &str) -> &'static str {
    match value {
        "replace" => "Replace",
        "keep-both" | "keep_both" => "Keep both",
        _ => "Skip",
    }
}

/// The conflict-policy row at the foot of the transfers popover.
///
/// Clicking the policy cycles skip -> replace -> keep both, and the tick
/// toggles checksum verification. Both apply to every transfer queued from
/// here on.
fn conflict_policy_row(
    t: &theme::Palette,
    overwrite: &str,
    verify: bool,
    owner: &Entity<AdbShareApp>,
) -> gpui::Stateful<gpui::Div> {
    let next = match overwrite {
        "replace" => "keep-both",
        "keep-both" | "keep_both" => "skip",
        _ => "replace",
    };
    let next = next.to_string();
    let owner = owner.clone();
    let toggle = owner.clone();
    let current = overwrite.to_string();
    let was_verifying = verify;
    div()
        .id("conflict-policy")
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .border_t_1()
        .border_color(t.border_soft)
        .child(
            div()
                .flex_1()
                .text_size(px(11.0))
                .text_color(t.text_dim)
                .child("If the file exists"),
        )
        .child(
            div()
                .id("cycle-conflict-policy")
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(7.0))
                .h(px(24.0))
                .rounded(px(6.0))
                .bg(t.hover)
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(t.text_header)
                .hover(|s| s.bg(t.pressed))
                .child(policy_label(overwrite))
                .child(icons::icon(names::REFRESH, 11.0, t.text_muted))
                .on_click(move |_, window, cx| {
                    let next = next.clone();
                    let keep_verify = was_verifying;
                    owner.update(cx, |app, cx| {
                        daemon::set_transfer_policy(cx, &next, keep_verify);
                        app.toasts.push(
                            format!("Existing files: {}", policy_label(&next)),
                            ToastTone::Info,
                        );
                        cx.notify();
                    });
                    let _ = window;
                }),
        )
        .child(
            div()
                .id("toggle-verify")
                .flex()
                .items_center()
                .gap(px(5.0))
                .h(px(24.0))
                .px(px(7.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(if verify { t.text_header } else { t.text_muted })
                .hover(|s| s.bg(t.hover))
                .child(icons::icon(
                    names::CHECKBOX_CHECKED,
                    13.0,
                    if verify {
                        t.text_header
                    } else {
                        rgba(0x00000000)
                    },
                ))
                .child("Verify")
                .on_click(move |_, window, cx| {
                    let current = current.clone();
                    let now = !was_verifying;
                    toggle.update(cx, |app, cx| {
                        daemon::set_transfer_policy(cx, &current, now);
                        app.toasts.push(
                            format!("Checksum verification {}", if now { "on" } else { "off" }),
                            ToastTone::Info,
                        );
                        cx.notify();
                    });
                    let _ = window;
                }),
        )
}

/// The retry-failed row at the foot of the transfers popover.
fn retry_row(t: &theme::Palette, owner: &Entity<AdbShareApp>) -> gpui::Stateful<gpui::Div> {
    let owner = owner.clone();
    div()
        .id("retry-failed")
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .cursor_pointer()
        .hover(|s| s.bg(t.hover))
        .border_t_1()
        .border_color(t.border_soft)
        .child(icons::icon(names::EMBLEM_SYNC, 14.0, t.text_muted))
        .child(
            div()
                .flex_1()
                .text_size(px(11.0))
                .text_color(t.text_dim)
                .child("Retry failed transfers"),
        )
        .on_click(move |_, _w, cx| {
            owner.update(cx, |app, cx| {
                app.retry_failed(cx);
            });
        })
}

impl Render for AdbShareApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The palette is `Copy`, so take a value: a borrow of `cx` would collide
        // with the closures below that need `&mut cx`.
        let t = *cx.theme();
        let viewport_w: f32 = f32::from(window.viewport_size().width);
        self.layout = Layout::for_width(viewport_w);

        // The window title says where the user is, or which dialog is in the
        // way. Both matter once more than one window is open.
        let heading = self.dialog.heading();
        if heading.is_empty() {
            let target = if self.browser.read(cx).has_device() {
                let name = self.browser.read(cx).path().display().to_string();
                match name.rsplit('/').find(|part| !part.is_empty()) {
                    Some(last) if name != "/" => format!("{last} — ADBShare"),
                    _ => "ADBShare".to_string(),
                }
            } else {
                "ADBShare".to_string()
            };
            window.set_window_title(&target);
        } else {
            window.set_window_title(heading);
        }

        let show_sidebar = self.layout != Layout::Narrow && self.sidebar_visible;
        // The browser is told where its item area is, so rubber-band selection
        // hit-tests against real geometry.
        let sidebar_width = if show_sidebar {
            self.sidebar_width
        } else {
            0.0
        };
        let content_x = sidebar_width + if show_sidebar { 4.0 } else { 0.0 };
        let content_y = theme::TOPBAR_H;
        let viewport = window.viewport_size();
        let content_w: f32 = f32::from(viewport.width) - sidebar_width;
        // The browser gets the box it was given, not the item area inside it:
        // the context strip, search row and grid padding are the browser's own
        // children, and only it knows how tall they are.
        let content_h =
            f32::from(viewport.height) - theme::TOPBAR_H - theme::STATUSBAR_H - CONTEXT_BAR_H;
        self.browser.update(cx, |b, cx| {
            b.set_viewport(content_x, content_y, content_w, content_h, cx)
        });

        // The divider sits between the sidebar and the browser, so the file area
        // starts after it.
        let divider = show_sidebar.then(|| self.sidebar_divider(&t, cx));

        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(t.canvas)
            .when(show_sidebar, |d| d.child(self.sidebar(&t, cx)))
            .when_some(divider, |d, divider| d.child(divider))
            .child(self.browser.clone());

        // `cx.listener` borrows the context, which the render tree below also
        // needs, so the emitters are rebuilt from a weak entity instead.
        let me = cx.entity();
        let on_toast = {
            let me = me.clone();
            move |id: ActionId, _window: &mut Window, app: &mut App| {
                me.update(app, |this, cx| this.on_toast_action(id, cx));
            }
        };
        let on_dialog = {
            let me = me.clone();
            move |result: DialogResult, _window: &mut Window, app: &mut App| {
                me.update(app, |this, cx| this.on_dialog_result(&result, cx));
            }
        };
        let _on_menu = {
            let me = me.clone();
            move |id: &'static str, _window: &mut Window, app: &mut App| {
                me.update(app, |this, cx| this.on_menu_select(id, cx));
            }
        };
        let toasts = self.toasts.render(&t, on_toast);

        let capture = if self.resizing_sidebar {
            Some(self.resize_capture(window, cx))
        } else {
            None
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.canvas)
            .text_color(t.text_primary)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &DismissOverlays, _w, cx| {
                this.context_menu = None;
                this.overflow_open = false;
                this.transfers_open = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowAbout, _w, cx| {
                this.dialog = crate::dialogs::Dialog::About;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowDiagnostics, _w, cx| {
                this.open_diagnostics(cx);
            }))
            .on_action(cx.listener(|this, _: &ConnectWifi, _w, cx| {
                this.on_menu_select("connect-wifi", cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _w, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .child(self.topbar(&t, cx))
            .child(self.search_row(&t, cx))
            .child(body)
            .child(self.context_popover(&t, cx))
            .when_some(toasts, |d, toasts| d.child(toasts))
            .when_some(
                crate::dialogs::render(&self.dialog, &t, on_dialog),
                |d, dialog| d.child(dialog),
            )
            .when_some(capture, |d, capture| d.child(capture))
            .into_any_element()
    }
}

impl AdbShareApp {
    /// The search strip, shown only while search is active.
    fn search_row(&mut self, t: &theme::Palette, cx: &mut Context<Self>) -> AnyElement {
        if !self.browser.read(cx).search_active() {
            return div().into_any_element();
        }
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(14.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(t.border_soft)
            .child(icons::icon(names::EDIT_FIND, 14.0, t.text_muted))
            .child(self.search_field.clone())
            .child(ui::shortcut_label(t, "Esc to close", false))
            .into_any_element()
    }
}

/// What the shared name field is collecting.
enum NamePurpose {
    CreateFolder,
    Rename(DirEntry),
}

/// A path's last component, or a fallback when it has none.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "untitled".to_string())
}

/// Describe a selection for a dialog title.
fn describe_selection(entries: &[DirEntry]) -> String {
    match entries {
        [] => String::new(),
        [one] => one.name.clone(),
        many => format!("{} items", many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// Build the real root view, so the daemon-failure path is exercised through
    /// the same code the app runs.
    fn open(cx: &mut TestAppContext) -> Entity<AdbShareApp> {
        cx.update(|app| {
            theme::install(app);
            app.set_global(crate::protocol::TransferPolicy::default());
        });
        let saved = Preferences::default();
        cx.new(|cx| AdbShareApp::new(cx, &saved))
    }

    #[gpui::test]
    async fn a_single_poll_failure_is_not_reported(cx: &mut TestAppContext) {
        // The daemon is usually still starting when the GUI comes up; one
        // failure is not worth alarming anyone about.
        let app = open(cx);
        cx.update(|cx| {
            let this = app.read(cx);
            assert!(this.daemon_error.is_none());
        });
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.on_devices_result(Err("bus name has no owner".into()), cx)
            });
            let this = app.read(cx);
            assert_eq!(this.daemon_failures, 1);
            assert!(
                this.daemon_error.is_none(),
                "one failure is not enough to report"
            );
            assert!(this.toasts.is_empty(), "and nothing should be shown");
        });
    }

    #[gpui::test]
    async fn a_repeated_failure_says_the_daemon_is_unreachable(cx: &mut TestAppContext) {
        // An empty device list looks the same whether no phone is plugged in or
        // the daemon is dead, so the second failure has to say which it is.
        let app = open(cx);
        for _ in 0..2 {
            cx.update(|cx| {
                app.update(cx, |this, cx| {
                    this.on_devices_result(Err("bus name has no owner".into()), cx)
                });
            });
        }
        cx.update(|cx| {
            let this = app.read(cx);
            assert_eq!(this.daemon_failures, 2);
            assert_eq!(
                this.daemon_error.as_deref(),
                Some("bus name has no owner"),
                "the reason is kept so the sidebar can show it"
            );
            assert_eq!(this.toasts.len(), 1, "the user is told once, not per poll");
            assert!(!this.toasts.is_empty());
        });
    }

    #[gpui::test]
    async fn recovery_clears_the_daemon_error(cx: &mut TestAppContext) {
        let app = open(cx);
        for _ in 0..2 {
            cx.update(|cx| {
                app.update(cx, |this, cx| {
                    this.on_devices_result(Err("gone".into()), cx)
                });
            });
        }
        cx.update(|cx| {
            app.update(cx, |this, cx| this.on_devices_result(Ok(Vec::new()), cx));
            let this = app.read(cx);
            assert!(this.daemon_error.is_none(), "a good poll clears the error");
            assert_eq!(this.daemon_failures, 0, "and the counter resets");
        });
    }

    #[gpui::test]
    async fn a_failure_after_working_reports_immediately(cx: &mut TestAppContext) {
        // Losing a daemon that was working is news on the first poll, not the
        // second: the user was mid-transfer.
        let app = open(cx);
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.on_devices_result(Ok(vec!["serial".into()]), cx)
            });
        });
        // A successful poll fans out to `device_info` before the list is
        // populated; let that settle so the test really is starting from a
        // working daemon.
        for _ in 0..8 {
            cx.run_until_parked();
        }
        cx.update(|cx| {
            let this = app.read(cx);
            assert!(
                !this.devices.is_empty(),
                "precondition: a device is listed, so the daemon was working"
            );
        });
        cx.update(|cx| {
            app.update(cx, |this, cx| {
                this.on_devices_result(Err("gone".into()), cx)
            });
            let this = app.read(cx);
            assert_eq!(this.daemon_failures, 1);
            assert_eq!(this.daemon_error.as_deref(), Some("gone"));
            assert!(!this.toasts.is_empty());
            assert!(this.selection.is_none(), "the selection is dropped");
        });
    }
}

#[cfg(test)]
mod whole_window_bench {
    use super::*;
    use gpui::TestAppContext;

    /// Wraps an entity so a test window can render it.
    ///
    /// `add_window` needs a `Render`, and in GPUI 0.2.2 an `Entity<V>` is an
    /// element rather than a `Render`, so the adapter is explicit.
    struct RootView(Entity<AdbShareApp>);

    impl Render for RootView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.0.clone()
        }
    }

    /// Lay the browser out, so the browser benchmarks are comparable.
    ///
    /// `None` leaves the browser on its empty state, which is what isolates the
    /// chrome's cost from the browser's.
    fn populate(cx: &mut TestAppContext, view: &Entity<AdbShareApp>, count: Option<usize>) {
        let Some(count) = count else { return };
        cx.update(|app| {
            view.update(app, |this, cx| {
                this.select_local(std::path::Path::new("/usr/lib"), cx);
            });
        });
        cx.update(|app| {
            let browser = this_browser(app, view);
            browser.update(app, |b, cx| {
                b.install_entries(crate::browser::synthetic_listing(count));
                cx.notify();
            });
        });
    }

    fn this_browser(app: &mut App, view: &Entity<AdbShareApp>) -> Entity<Browser> {
        view.read(app).browser.clone()
    }

    /// Time a full layout pass of the whole window, not just the browser.
    ///
    /// The browser benchmarks measure the pane; this measures what the user
    /// actually waits for, so a regression in the top bar, the sidebar or the
    /// breadcrumb trail would show up here rather than hiding.
    fn bench_window(cx: &mut TestAppContext, count: Option<usize>) -> f64 {
        cx.update(crate::theme::install);
        cx.update(|app| app.set_global(crate::protocol::TransferPolicy::default()));
        let handle = cx.add_window(|window, app| {
            let saved = Preferences::default();
            RootView(AdbShareApp::new_entity(window, app, &saved))
        });
        let root = cx.update(|app| handle.root(app).expect("root view"));
        let view = cx.update(|app| root.read(app).0.clone());
        populate(cx, &view, count);
        let vctx = cx.add_empty_window();

        // Draw into the size the app root actually believes it has. It derives
        // the browser's viewport from `window.viewport_size()`, so measuring it
        // against any other box makes the grid build the wrong number of tiles —
        // 252 instead of about 70 when the draw space was smaller than the test
        // window, which put the whole-window figure three times out.
        let space = vctx.update(|window, _| {
            gpui::size(window.viewport_size().width, window.viewport_size().height)
        });
        let mut best = f64::MAX;
        for _ in 0..12 {
            let start = std::time::Instant::now();
            let _ = vctx.draw(gpui::point(px(0.), px(0.)), space, |_w, _cx| view.clone());
            best = best.min(start.elapsed().as_secs_f64() * 1000.0);
        }
        let tiles = cx.update(|app| this_browser(app, &view).read(app).built_tiles());
        // Printed so the figure can be read: the harness gives the test window
        // the full test display, so this measures a much larger window than the
        // app opens at, and the tile count scales with it.
        let (w, h): (f32, f32) = (space.width.into(), space.height.into());
        println!("    window {w:.0}x{h:.0}, {tiles} tiles built");
        best
    }

    /// The cost of a whole frame, top bar and sidebar included.
    ///
    /// The test harness sizes its window to the full test display, which is much
    /// larger than the 1000x680 the app opens at, so these absolute figures are
    /// an upper bound rather than what a user sees. The per-tile cost they imply
    /// is the useful part: it matches the browser-only benchmark, which is the
    /// evidence that the whole-window figure is just tiles.
    #[ignore = "benchmark"]
    #[gpui::test]
    async fn whole_window_layout_cost(_cx: &mut TestAppContext) {
        // The chrome alone: no device, so the browser draws its empty state and
        // everything left is the top bar, the sidebar and the bars.
        let mut cx = TestAppContext::single();
        let chrome = bench_window(&mut cx, None);
        println!("bench window chrome only    {chrome:8.3} ms  (no browser)");
        for count in [200usize, 1_000, 5_000] {
            let mut cx = TestAppContext::single();
            let ms = bench_window(&mut cx, Some(count));
            println!(
                "bench window n={count:<6}      {ms:8.3} ms  (+{:.3} for the browser)",
                ms - chrome
            );
        }
    }
}
