//! Application root: AdwApplication + main window + D-Bus client.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use std::os::unix::fs::PermissionsExt;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::{prelude::*, *};

use crate::device_list::{DeviceList, SidebarEvent};
use crate::file_browser::ViewMode;
use crate::file_browser::{BrowserEvent, DirEntry as FsDirEntry, FileBrowser};
use crate::transfer_dock::TransferDock;
use crate::transfer_view::{JobInfo, TransferView};

#[zbus::proxy(
    default_service = "org.adbshare.Manager",
    interface = "org.adbshare.Manager",
    default_path = "/org/adbshare/Manager"
)]
trait Manager {
    async fn list_devices(&self) -> zbus::Result<Vec<String>>;
    async fn device_info(&self, serial: &str) -> zbus::Result<String>;
    async fn adb_version(&self) -> zbus::Result<String>;
    async fn list_dir(&self, device: &str, path: &str) -> zbus::Result<String>;
    async fn enqueue_push(&self, device: &str, local_path: &str, device_path: &str) -> zbus::Result<u64>;
    async fn enqueue_pull(&self, device: &str, device_path: &str, local_path: &str) -> zbus::Result<u64>;
    async fn list_jobs(&self) -> zbus::Result<String>;
    async fn pause_job(&self, id: u64) -> zbus::Result<bool>;
    async fn resume_job(&self, id: u64) -> zbus::Result<bool>;
    async fn cancel_job(&self, id: u64) -> zbus::Result<bool>;
    async fn mkdir(&self, device: &str, path: &str) -> zbus::Result<()>;
    async fn rename(&self, device: &str, src: &str, dst: &str) -> zbus::Result<()>;
    async fn delete(&self, device: &str, path: &str) -> zbus::Result<()>;
    async fn connect_wireless(&self, address: &str) -> zbus::Result<String>;
    async fn mountpoint_for(&self, device: &str) -> zbus::Result<String>;
}

mod adbshare_dbus_proxy {
    pub use super::ManagerProxy;
}

struct UiHandles {
    browser: FileBrowser,
    transfer: TransferView,
    dock: TransferDock,
    selected_device: parking_lot::Mutex<Option<String>>,
    /// Cache of live device metadata from the daemon.
    devices: parking_lot::Mutex<Vec<crate::device_list::DeviceEntry>>,
    /// Ids of jobs currently Pending/Running (for banner Pause/Cancel).
    active_jobs: parking_lot::Mutex<Vec<u64>>,
    /// Whether the banner pause button currently means "resume".
    transfers_paused: parking_lot::Mutex<bool>,
}

/// Sidebar geometry: the divider defaults to this and can be dragged down to
/// the sidebar's 240px minimum (its size request below). GTK exposes no
/// maximum for the divider, so it can also be dragged wider.
const SIDEBAR_DEFAULT_WIDTH: i32 = 248;

pub struct AdbshareApp {
    app: adw::Application,
}

impl AdbshareApp {
    pub fn new() -> Self {
        let app = adw::Application::builder()
            .application_id("org.adbshare.Gui")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        Self { app }
    }

    pub fn run(self) -> anyhow::Result<()> {
        let app = self.app;

        // Tokio runtime — zbus needs a reactor.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let rt_handle = rt.handle().clone();
        std::thread::Builder::new()
            .name("adbshare-tokio".into())
            .spawn(move || {
                let _ = rt.block_on(async {
                    std::future::pending::<()>().await;
                });
            })?;

        app.connect_activate(move |app| {
            // Load custom CSS stylesheet
            let css_provider = gtk4::CssProvider::new();
            css_provider.load_from_data(include_str!("style.css"));
            if let Some(display) = gdk4::Display::default() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &css_provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }

            let window = adw::ApplicationWindow::builder()
                .application(app)
                .title("ADBShare Files")
                .default_width(1280)
                .default_height(800)
                .build();
            window.add_css_class("background");

            // --- Layout: native header + sidebar/content body ---
            // One window, one header, one job list. The header owns navigation
            // (back/forward/up), the location (breadcrumbs), and the primary
            // actions (send/save/new folder/view/search/transfers/menu) so no
            // action is hidden behind a right-click.
            let main_layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

            let browser = FileBrowser::new();
            browser.root.set_vexpand(true);
            browser.root.set_hexpand(true);

            let header = adw::HeaderBar::new();

            // Left: back / forward / up.
            let nav_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
            nav_box.set_valign(gtk4::Align::Center);
            for btn in [&browser.back_button, &browser.forward_button, &browser.up_button] {
                btn.add_css_class("flat");
                btn.set_valign(gtk4::Align::Center);
                nav_box.append(btn);
            }
            header.pack_start(&nav_box);

            // Center: breadcrumbs (the location). Left-aligned inside the
            // title area so paths scan like a file manager.
            browser.path_stack.set_hexpand(true);
            browser.breadcrumb_container.set_halign(gtk4::Align::Start);
            header.set_title_widget(Some(&browser.path_stack));

            // Full transfer queue lives in one popover, opened from the
            // header badge or by clicking the bottom status bar.
            let transfer = TransferView::new();
            let trans_pop = gtk4::Popover::new();
            trans_pop.set_size_request(460, 340);
            let trans_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            let trans_hdr = gtk4::Label::builder()
                .label("Transfers")
                .xalign(0.0)
                .margin_start(14)
                .margin_top(10)
                .margin_bottom(6)
                .build();
            trans_hdr.add_css_class("heading");
            trans_box.append(&trans_hdr);
            let trans_sub = gtk4::Label::builder()
                .label("Copies between this computer and the phone.")
                .xalign(0.0)
                .margin_start(14)
                .margin_bottom(6)
                .wrap(true)
                .build();
            trans_sub.add_css_class("dim-label");
            trans_box.append(&trans_sub);
            transfer.transfer_attach(&trans_box);
            trans_pop.set_child(Some(&trans_box));

            // Right: primary actions first, overflow last.
            let header_end = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
            header_end.set_valign(gtk4::Align::Center);

            // "Send to phone" is the primary action.
            browser.upload_button.add_css_class("suggested-action");
            browser.upload_button.set_valign(gtk4::Align::Center);
            header_end.append(&browser.upload_button);
            browser.download_button.set_valign(gtk4::Align::Center);
            header_end.append(&browser.download_button);
            browser.new_folder_button.set_has_frame(false);
            // Icon-only in the header (the button already carries an
            // icon+label child; keep just the icon so the header stays roomy).
            browser.new_folder_button.set_child(Some(
                &gtk4::Image::from_icon_name("folder-new-symbolic"),
            ));
            browser.new_folder_button.set_tooltip_text(Some("New folder"));
            browser.new_folder_button.set_valign(gtk4::Align::Center);
            header_end.append(&browser.new_folder_button);

            // Grid <-> list. Icon always shows the CURRENT view; the tooltip
            // names the action, so it never reads backwards.
            let view_toggle = gtk4::Button::from_icon_name("view-grid-symbolic");
            view_toggle.add_css_class("flat");
            view_toggle.set_tooltip_text(Some("Switch to list view"));
            view_toggle.set_valign(gtk4::Align::Center);
            {
                let browser_vt = browser.clone();
                let vt = view_toggle.clone();
                view_toggle.connect_clicked(move |_| {
                    let new_mode = if browser_vt.view_mode() == ViewMode::Grid {
                        ViewMode::List
                    } else {
                        ViewMode::Grid
                    };
                    browser_vt.set_view_mode(new_mode);
                    if new_mode == ViewMode::Grid {
                        vt.set_icon_name("view-grid-symbolic");
                        vt.set_tooltip_text(Some("Switch to list view"));
                    } else {
                        vt.set_icon_name("view-list-symbolic");
                        vt.set_tooltip_text(Some("Switch to grid view"));
                    }
                });
            }
            header_end.append(&view_toggle);

            // Search toggle reveals the search row under the header.
            browser.search_button.set_valign(gtk4::Align::Center);
            header_end.append(&browser.search_button);

            // Transfers badge button: icon + live count.
            let transfers_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            let transfers_icon = gtk4::Image::from_icon_name("emblem-synchronizing-symbolic");
            transfers_box.append(&transfers_icon);
            let transfers_count = gtk4::Label::new(Some("Transfers"));
            transfers_box.append(&transfers_count);
            let transfers_btn = gtk4::MenuButton::new();
            transfers_btn.set_child(Some(&transfers_box));
            transfers_btn.set_tooltip_text(Some("Show transfers"));
            transfers_btn.set_valign(gtk4::Align::Center);
            transfers_btn.set_popover(Some(&trans_pop));
            header_end.append(&transfers_btn);

            // Overflow menu: secondary actions only. Primary actions already
            // have header buttons, so they are NOT duplicated here.
            let kebab = gtk4::MenuButton::new();
            // Declared before the menu items so their handlers can pop it
            // down after activation.
            let kebab_pop = gtk4::Popover::new();
            kebab.set_icon_name("view-more-symbolic");
            kebab.add_css_class("flat");
            kebab.set_tooltip_text(Some("More options"));
            kebab.set_valign(gtk4::Align::Center);
            let kebab_menu = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            kebab_menu.set_margin_top(6);
            kebab_menu.set_margin_bottom(6);
            kebab_menu.set_margin_start(6);
            kebab_menu.set_margin_end(6);
            kebab_menu.set_size_request(250, -1);
            let menu_item = |icon: &str, label: &str| {
                let btn = gtk4::Button::new();
                btn.set_has_frame(false);
                let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
                hbox.set_margin_start(6);
                hbox.set_margin_end(6);
                hbox.append(&gtk4::Image::from_icon_name(icon));
                let lbl = gtk4::Label::builder().label(label).xalign(0.0).hexpand(true).build();
                hbox.append(&lbl);
                btn.set_child(Some(&hbox));
                btn
            };
            let refresh_item = menu_item("view-refresh-symbolic", "Refresh");
            {
                let rb = browser.refresh_button.clone();
                let kp = kebab_pop.clone();
                refresh_item.connect_clicked(move |_| {
                    kp.popdown();
                    rb.emit_clicked();
                });
            }
            kebab_menu.append(&refresh_item);
            let select_all_item = menu_item("edit-select-all-symbolic", "Select all");
            {
                let browser_sa = browser.clone();
                let kp = kebab_pop.clone();
                select_all_item.connect_clicked(move |_| {
                    kp.popdown();
                    browser_sa.select_all_active();
                });
            }
            kebab_menu.append(&select_all_item);
            kebab_menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
            let files_item = menu_item("system-file-manager-symbolic", "Open in Files");
            {
                let browser_f = browser.clone();
                let kp = kebab_pop.clone();
                files_item.connect_clicked(move |_| {
                    kp.popdown();
                    browser_f.emit(crate::file_browser::BrowserEvent::OpenExternal(browser_f.current_path()));
                });
            }
            kebab_menu.append(&files_item);
            let terminal_item = menu_item("utilities-terminal-symbolic", "Open in terminal");
            {
                let browser_t = browser.clone();
                let kp = kebab_pop.clone();
                terminal_item.connect_clicked(move |_| {
                    kp.popdown();
                    browser_t.emit(crate::file_browser::BrowserEvent::OpenTerminal(browser_t.current_path()));
                });
            }
            kebab_menu.append(&terminal_item);
            let hidden_check = gtk4::CheckButton::builder()
                .label("Show hidden files")
                .margin_start(6)
                .margin_end(6)
                .build();
            {
                let browser_h = browser.clone();
                let kp = kebab_pop.clone();
                hidden_check.connect_toggled(move |btn| {
                    // Choosing a toggle inside the menu closes it; re-opening
                    // re-reads the live state.
                    kp.popdown();
                    if btn.is_active() != browser_h.show_hidden() {
                        browser_h.toggle_show_hidden();
                    }
                });
            }
            kebab_menu.append(&hidden_check);
            kebab_menu.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
            let app_about = menu_item("help-about-symbolic", "About ADBShare");
            {
                let win_about = window.clone();
                let kp = kebab_pop.clone();
                app_about.connect_clicked(move |_| {
                    kp.popdown();
                    let dialog = adw::MessageDialog::builder()
                        .heading("ADBShare Files")
                        .body(format!(
                            "Copy files between Linux and Android over ADB.\nVersion {} (pre-alpha)",
                            env!("CARGO_PKG_VERSION")
                        ))
                        .modal(true)
                        .transient_for(&win_about)
                        .build();
                    dialog.add_response("ok", "Close");
                    dialog.present();
                });
            }
            kebab_menu.append(&app_about);
            kebab_pop.set_child(Some(&kebab_menu));
            kebab.set_popover(Some(&kebab_pop));
            header_end.append(&kebab);

            header.pack_end(&header_end);
            main_layout.append(&header);

            // Search row: hidden until the header search toggle is on.
            browser.search_entry.set_placeholder_text(Some("Search this folder..."));
            browser.search_entry.set_hexpand(true);
            let search_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            search_row.set_margin_start(12);
            search_row.set_margin_end(12);
            search_row.set_margin_top(6);
            search_row.set_margin_bottom(6);
            search_row.append(&browser.search_entry);
            browser.search_bar.set_child(Some(&search_row));
            main_layout.append(&browser.search_bar);

            // --- Body: resizable sidebar + content ---
            let body_paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
            body_paned.set_vexpand(true);
            body_paned.set_hexpand(true);
            body_paned.add_css_class("sidebar-paned");

            let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            sidebar_box.add_css_class("navigation-sidebar");
            sidebar_box.set_size_request(240, -1);
            body_paned.set_start_child(Some(&sidebar_box));
            body_paned.set_shrink_start_child(true);

            let device_list = std::rc::Rc::new(DeviceList::new());
            device_list.populate_defaults();
            device_list.attach(&sidebar_box);

            let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            content_box.set_vexpand(true);
            content_box.set_hexpand(true);
            content_box.append(&browser.root);
            body_paned.set_end_child(Some(&content_box));
            body_paned.set_position(SIDEBAR_DEFAULT_WIDTH);
            main_layout.append(&body_paned);

            // Bottom status bar owns transfer progress; the small dock card
            // only appears while jobs run and opens the full list on click.
            let window_overlay = gtk4::Overlay::new();
            window_overlay.set_child(Some(&main_layout));

            let dock = TransferDock::new();
            window_overlay.add_overlay(&dock.root);
            window.set_content(Some(&window_overlay));

            // Drop files from other apps onto the canvas -> push/copy into
            // the directory being browsed.
            let (drop_tx, drop_rx) = async_channel::unbounded::<(PathBuf, Vec<PathBuf>)>();
            // Accept both a single gio::File and a GdkFileList: multi-file
            // drags (e.g. from Nautilus) deliver a GdkFileList, and matching
            // only the File GType dropped every file but the first.
            let drop_target = gtk4::DropTarget::new(gtk4::gio::File::static_type(), gdk4::DragAction::COPY);
            drop_target.set_types(&[gtk4::gio::File::static_type(), gdk4::FileList::static_type()]);
            let browser_drop = browser.clone();
            drop_target.connect_drop(move |_target, value, _x, _y| {
                let mut paths: Vec<PathBuf> = Vec::new();
                if let Ok(list) = value.get::<gdk4::FileList>() {
                    paths.extend(list.files().iter().filter_map(|f| f.path()));
                } else if let Ok(file) = value.get::<gtk4::gio::File>() {
                    paths.extend(file.path());
                }
                if paths.is_empty() {
                    return false;
                }
                let curr = browser_drop.current_path();
                let _ = drop_tx.try_send((curr, paths));
                true
            });
            browser.root.add_controller(drop_target);

            // Stash handles
            let handles = std::rc::Rc::new(UiHandles {
                browser,
                transfer,
                dock,
                selected_device: parking_lot::Mutex::new(None),
                devices: parking_lot::Mutex::new(Vec::new()),
                active_jobs: parking_lot::Mutex::new(Vec::new()),
                transfers_paused: parking_lot::Mutex::new(false),
            });

            // Dock controls reuse the banner's Pause/Cancel event flow;
            // clicking anywhere else on the dock opens the full
            // Operations & Transfers popover.
            {
                let browser_dock = handles.browser.clone();
                handles.dock.pause_button.connect_clicked(move |_| {
                    browser_dock.emit(BrowserEvent::PauseTransfer);
                });
            }
            {
                let browser_dock = handles.browser.clone();
                handles.dock.cancel_button.connect_clicked(move |_| {
                    browser_dock.emit(BrowserEvent::CancelTransfer);
                });
            }
            {
                let tp = trans_pop.clone();
                let click = gtk4::GestureClick::new();
                // Primary button only: right-/middle-clicks must not open the
                // transfers popover.
                click.set_button(1);
                click.connect_released(move |_, _, _, _| tp.popup());
                handles.dock.root.add_controller(click);
            }

            // --- D-Bus channels ---
            let (devices_tx, devices_rx) = async_channel::unbounded::<Result<Vec<crate::device_list::DeviceEntry>, String>>();
            let (dir_tx, dir_rx) = async_channel::unbounded::<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>();
            let (jobs_tx, jobs_rx) = async_channel::unbounded::<Result<Vec<JobInfo>, String>>();
            let (info_tx, info_rx) = async_channel::unbounded::<(String, Result<String, String>)>();
            let (mp_tx, mp_rx) = async_channel::unbounded::<Result<String, String>>();
            // File-operation results: Err -> error dialog; Ok -> optional info dialog.
            let (op_tx, op_rx) = async_channel::unbounded::<(Option<String>, Result<String, String>)>();

            // --- Periodic device poll (replaces the one-shot initial fetch) ---
            {
                let rt_poll = rt_handle.clone();
                let devices_tx_poll = devices_tx.clone();
                glib::spawn_future_local(async move {
                    loop {
                        let tx = devices_tx_poll.clone();
                        rt_poll.spawn(async move {
                            let res = fetch_devices().await;
                            let _ = tx.send(res).await;
                        });
                        glib::timeout_future(Duration::from_secs(3)).await;
                    }
                });
            }

            // --- Drain device-info refreshes -> cache + banner ---
            let handles_info = handles.clone();
            glib::spawn_future_local(async move {
                while let Ok((serial, res)) = info_rx.recv().await {
                    if let Ok(json) = res {
                        if let Ok(dto) = serde_json::from_str::<DeviceInfoDto>(&json) {
                            let entry = dto.into_entry();
                            {
                                let mut list = handles_info.devices.lock();
                                if let Some(slot) = list.iter_mut().find(|d| d.serial == serial) {
                                    *slot = entry.clone();
                                }
                            }
                            handles_info.browser.set_device_info(&entry);
                        }
                    }
                }
            });

            // --- Sidebar Events (Device / Place selected) ---
            let handles_sidebar = handles.clone();
            let dir_tx_sidebar = dir_tx.clone();
            let info_tx_sidebar = info_tx.clone();
            let mp_tx_sidebar = mp_tx.clone();
            let op_tx_sidebar = op_tx.clone();
            let rt_sidebar = rt_handle.clone();
            let window_for_sidebar = window.clone();
            device_list.on_event(move |ev| match ev {
                SidebarEvent::SelectDevice(serial) => {
                    select_device(&serial, &handles_sidebar, &dir_tx_sidebar, &info_tx_sidebar, &mp_tx_sidebar, rt_sidebar.clone());
                }
                SidebarEvent::SelectLocal(path) => {
                    if !path.is_dir() {
                        let _ = op_tx_sidebar.try_send((None, Err(format!("{} does not exist (is the folder or Trash empty?)", path.display()))));
                        return;
                    }
                    handles_sidebar.browser.set_local_mode();
                    handles_sidebar.browser.set_loading(true);
                    let dir_tx = dir_tx_sidebar.clone();
                    let p = path.clone();
                    rt_sidebar.spawn_blocking(move || {
                        let res = list_local_dir(&p);
                        let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), p, res));
                    });
                }
                SidebarEvent::ConnectIp => {
                    show_connect_dialog(&window_for_sidebar, op_tx_sidebar.clone(), rt_sidebar.clone());
                }
                SidebarEvent::SelectPlace(path) => {
                    let device = match handles_sidebar.selected_device.lock().clone() {
                        Some(d) => d,
                        None => {
                            let _ = op_tx_sidebar.try_send((
                                None,
                                Err("No device selected — connect an Android device first.".to_string()),
                            ));
                            return;
                        }
                    };
                    handles_sidebar.browser.set_loading(true);
                    let dir_tx = dir_tx_sidebar.clone();
                    let path_str = path.to_string_lossy().to_string();
                    let dev_clone = device.clone();
                    let target_path = path.clone();
                    rt_sidebar.spawn(async move {
                        let res = list_dir(&dev_clone, &path_str).await.map_err(|e| e.to_string());
                        let _ = dir_tx.send((dev_clone, target_path, res)).await;
                    });
                }
            });

            // --- Drain device list -> sidebar ---
            let dev_list_drain = device_list.clone();
            let handles_dev_drain = handles.clone();
            let dir_tx_dev_drain = dir_tx.clone();
            let info_tx_dev_drain = info_tx.clone();
            let mp_tx_dev_drain = mp_tx.clone();
            let rt_dev_drain = rt_handle.clone();
            glib::spawn_future_local(async move {
                while let Ok(result) = devices_rx.recv().await {
                    match result {
                        Ok(devices) => {
                            *handles_dev_drain.devices.lock() = devices.clone();
                            let selected = handles_dev_drain.selected_device.lock().clone();
                            dev_list_drain.set_devices(&devices, selected.as_deref());
                            // The selected device disappeared (unplugged /
                            // daemon lost it): drop it as the active selection
                            // and reset the browser instead of silently
                            // re-highlighting a different row. Re-plugging the
                            // same device works via a normal sidebar click (or
                            // the auto-select below once nothing is selected).
                            if let Some(ref sel) = selected {
                                if !devices.iter().any(|d| &d.serial == sel) {
                                    *handles_dev_drain.selected_device.lock() = None;
                                    if !handles_dev_drain.browser.is_local_mode() {
                                        handles_dev_drain.browser.set_device(None);
                                    }
                                }
                            }
                            // Auto-select the first real device if none selected.
                            if selected.is_none() {
                                if let Some(first) = devices.first() {
                                    select_device(&first.serial, &handles_dev_drain, &dir_tx_dev_drain, &info_tx_dev_drain, &mp_tx_dev_drain, rt_dev_drain.clone());
                                }
                            }
                        }
                        Err(_e) => {
                            // Daemon unreachable — keep the empty state.
                        }
                    }
                }
            });

            // --- Browser events ---
            let handles_browser = handles.clone();
            let dir_tx_browser = dir_tx.clone();
            let op_tx_browser = op_tx.clone();
            let rt_browser = rt_handle.clone();
            let window_for_dialogs = window.clone();
            handles.browser.on_event(move |ev| {
                handle_browser_event(
                    ev,
                    &handles_browser,
                    &dir_tx_browser,
                    &op_tx_browser,
                    rt_browser.clone(),
                    window_for_dialogs.clone(),
                );
            });

            // --- Drain dir results ---
            let handles_dir = handles.clone();
            let op_tx_dir = op_tx.clone();
            glib::spawn_future_local(async move {
                while let Ok((serial, path, result)) = dir_rx.recv().await {
                    // Drop listings that no longer match what the user is
                    // looking at: a slow listing from device A (or from local
                    // mode) must not overwrite the view after the user
                    // switched to device B (or to a device from local mode).
                    let expected = if handles_dir.browser.is_local_mode() {
                        LOCAL_DEVICE.to_string()
                    } else {
                        match handles_dir.selected_device.lock().clone() {
                            Some(d) => d,
                            // Nothing selected: nothing may claim the view.
                            None => continue,
                        }
                    };
                    if serial != expected {
                        tracing::debug!(stale = %serial, current = %expected, "dropped stale dir listing");
                        continue;
                    }
                    match result {
                        Ok(entries) => {
                            handles_dir.browser.show_path(path);
                            handles_dir.browser.set_entries(entries);
                            handles_dir.browser.show_list();
                            handles_dir.browser.set_loading(false);
                        }
                        Err(e) => {
                            handles_dir.browser.set_loading(false);
                            tracing::warn!(error=%e, "list_dir failed");
                            // Surface the failure: without this the user only
                            // saw the spinner stop, with the old listing and
                            // breadcrumbs left dangling.
                            let _ = op_tx_dir.try_send((Some("Could not open folder".to_string()), Err(e)));
                        }
                    }
                }
            });

            // --- Periodic job refresh ---
            {
                let rt_poll = rt_handle.clone();
                let jobs_tx_poll = jobs_tx.clone();
                glib::spawn_future_local(async move {
                    loop {
                        let tx = jobs_tx_poll.clone();
                        rt_poll.spawn(async move {
                            let res = list_jobs().await.map_err(|e| e.to_string());
                            let _ = tx.send(res).await;
                        });
                        glib::timeout_future(Duration::from_millis(600)).await;
                    }
                });
            }

            // --- Drain drop events -> DropFiles handling (move/copy/push) ---
            {
                let handles_drop = handles.clone();
                let dir_tx_drop = dir_tx.clone();
                let op_tx_drop = op_tx.clone();
                let rt_drop = rt_handle.clone();
                let window_drop = window.clone();
                glib::spawn_future_local(async move {
                    while let Ok((target_dir, files)) = drop_rx.recv().await {
                        handle_browser_event(
                            BrowserEvent::DropFiles { from_dir: target_dir.clone(), target_dir, files },
                            &handles_drop,
                            &dir_tx_drop,
                            &op_tx_drop,
                            rt_drop.clone(),
                            window_drop.clone(),
                        );
                    }
                });
            }

            // --- Drain mountpoint results -> drag & drop out of the app ---
            {
                let browser_mp = handles.browser.clone();
                glib::spawn_future_local(async move {
                    while let Ok(res) = mp_rx.recv().await {
                        match res {
                            Ok(mp) => browser_mp.set_fuse_mount(Some(&mp)),
                            Err(e) => tracing::warn!(error=%e, "mountpoint_for failed"),
                        }
                    }
                });
            }

            // --- Drain operation results -> dialogs the user can act on ---
            {
                let window_op = window.clone();
                glib::spawn_future_local(async move {
                    while let Ok((title, res)) = op_rx.recv().await {
                        match res {
                            Ok(msg) => {
                                let dialog = adw::MessageDialog::builder()
                                    .heading(title.as_deref().unwrap_or("Done"))
                                    .body(&msg)
                                    .modal(true)
                                    .transient_for(&window_op)
                                    .build();
                                dialog.add_response("ok", "OK");
                                dialog.present();
                            }
                            Err(msg) => show_error_dialog(&window_op, title.as_deref().unwrap_or("Something went wrong"), &msg),
                        }
                    }
                });
            }

            // --- Drain jobs -> status bar, dock card, header count, full list ---
            // One job list, three views of it. The status bar owns progress.
            let handles_jobs = handles.clone();
            let browser_drain = handles.browser.clone();
            let count_drain = transfers_count.clone();
            glib::spawn_future_local(async move {
                while let Ok(result) = jobs_rx.recv().await {
                    match result {
                        Ok(jobs) => {
                            // Paused jobs count as active: they must stay
                            // visible (banner + dock) and resumable.
                            let active: Vec<_> = jobs.iter().filter(|j| {
                                j.state == "Running" || j.state == "Pending" || j.state == "Paused"
                            }).collect();
                            *handles_jobs.active_jobs.lock() = active.iter().map(|j| j.id).collect();
                            if active.len() == 1 {
                                count_drain.set_label("1 transfer");
                            } else if active.is_empty() {
                                count_drain.set_label("Transfers");
                            } else {
                                count_drain.set_label(&format!("{} transfers", active.len()));
                            }
                            if !active.is_empty() {
                                let total_speed: u64 = active.iter().map(|j| j.speed_bps).sum();
                                let transferred_bytes: u64 = active.iter().map(|j| j.bytes_done).sum();
                                let total_bytes: u64 = active.iter().map(|j| j.bytes_total).sum();

                                let speed_mb = (total_speed as f64) / 1_048_576.0;
                                let trans_mb = (transferred_bytes as f64) / 1_048_576.0;
                                let tot_mb = (total_bytes as f64) / 1_048_576.0;

                                let fraction = if total_bytes > 0 {
                                    ((transferred_bytes as f64) / (total_bytes as f64)).clamp(0.0, 1.0)
                                } else {
                                    0.5
                                };

                                let banner_str = if tot_mb > 0.0 {
                                    format!(
                                        "Copying {} file{} — {:.1} of {:.1} MB ({:.1} MB/s)",
                                        active.len(),
                                        if active.len() > 1 { "s" } else { "" },
                                        trans_mb,
                                        tot_mb,
                                        speed_mb
                                    )
                                } else {
                                    format!(
                                        "Copying {} file{} — {:.1} MB/s",
                                        active.len(),
                                        if active.len() > 1 { "s" } else { "" },
                                        speed_mb
                                    )
                                };
                                browser_drain.update_transfer_banner(true, &banner_str, fraction);
                            } else {
                                browser_drain.update_transfer_banner(false, "", 0.0);
                                // Nothing left to resume; reset the toggle so
                                // the next transfer starts in "pause" state.
                                *handles_jobs.transfers_paused.lock() = false;
                            }
                            // The pause button means "resume" while paused.
                            let paused = *handles_jobs.transfers_paused.lock();
                            let (icon, tip) = if paused {
                                ("media-playback-start-symbolic", "Resume all transfers")
                            } else {
                                ("media-playback-pause-symbolic", "Pause all transfers")
                            };
                            browser_drain.banner_pause_btn.set_icon_name(icon);
                            browser_drain.banner_pause_btn.set_tooltip_text(Some(tip));
                            handles_jobs.dock.update(&jobs, paused);
                            handles_jobs.transfer.update_jobs(jobs);
                        }
                        Err(e) => tracing::warn!(error=%e, "list_jobs failed"),
                    }
                }
            });

            window.present();
        });

        app.run_with_args::<&str>(&[]);
        Ok(())
    }
}

/// Live device metadata as reported by the daemon over D-Bus.
#[derive(Debug, Clone, serde::Deserialize)]
struct DeviceInfoDto {
    serial: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    android_version: Option<String>,
    transport: String,
    #[serde(default)]
    battery_pct: Option<u8>,
    #[serde(default)]
    storage_used: Option<u64>,
    #[serde(default)]
    storage_total: Option<u64>,
}

impl DeviceInfoDto {
    fn into_entry(self) -> crate::device_list::DeviceEntry {
        crate::device_list::DeviceEntry {
            serial: self.serial,
            model: self.model,
            transport: if self.transport == "wifi" { "wifi" } else { "usb" },
            storage: match (self.storage_used, self.storage_total) {
                (Some(u), Some(t)) if t > 0 => Some((u, t)),
                _ => None,
            },
            battery_pct: self.battery_pct,
        }
    }
}

/// Query the daemon for devices + their live ADB metadata.
async fn fetch_devices() -> Result<Vec<crate::device_list::DeviceEntry>, String> {
    let serials = list_devices().await.map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(serials.len());
    for s in serials {
        let entry = match device_info(&s).await {
            Ok(json) => serde_json::from_str::<DeviceInfoDto>(&json)
                .map(DeviceInfoDto::into_entry)
                .unwrap_or_else(|_| fallback_entry(&s)),
            Err(_) => fallback_entry(&s),
        };
        out.push(entry);
    }
    Ok(out)
}

fn fallback_entry(serial: &str) -> crate::device_list::DeviceEntry {
    crate::device_list::DeviceEntry {
        serial: serial.to_string(),
        model: None,
        transport: "usb",
        storage: None,
        battery_pct: None,
    }
}

/// Shared "user picked (or auto-picked) this device" flow: updates header,
/// banner, browser state, and kicks off a directory listing plus a fresh
/// `device_info` fetch (battery/storage change over time).
fn select_device(
    serial: &str,
    handles: &std::rc::Rc<UiHandles>,
    dir_tx: &async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
    info_tx: &async_channel::Sender<(String, Result<String, String>)>,
    mp_tx: &async_channel::Sender<Result<String, String>>,
    rt: tokio::runtime::Handle,
) {
    {
        let mut sel = handles.selected_device.lock();
        *sel = Some(serial.to_string());
    }
    let cached = handles.devices.lock().iter().find(|d| d.serial == serial).cloned();
    handles.browser.set_device(Some(serial));
    if let Some(ref entry) = cached {
        handles.browser.set_device_info(entry);
    }
    handles.browser.show_path(PathBuf::from("/sdcard/Download"));

    // Fetch the FUSE mountpoint for drag & drop out of the app; the result
    // is drained on the GTK side (browser isn't Send).
    {
        let mp_tx = mp_tx.clone();
        let serial_m = serial.to_string();
        rt.spawn(async move {
            let _ = mp_tx.send(mountpoint_for(&serial_m).await.map_err(|e| e.to_string())).await;
        });
    }

    // Refresh live info (battery/storage change over time); the result is
    // drained on the GTK side.
    {
        let info_tx = info_tx.clone();
        let serial_c = serial.to_string();
        rt.spawn(async move {
            let res = device_info(&serial_c).await.map_err(|e| e.to_string());
            let _ = info_tx.send((serial_c, res)).await;
        });
    }

    let dir_tx = dir_tx.clone();
    let serial_c = serial.to_string();
    rt.spawn(async move {
        let res = list_dir(&serial_c, "/sdcard/Download").await.map_err(|e| e.to_string());
        let _ = dir_tx.send((serial_c, PathBuf::from("/sdcard/Download"), res)).await;
    });
}

/// List a directory on the local Linux filesystem (sidebar "Linux Root").
fn list_local_dir(path: &std::path::Path) -> Result<Vec<FsDirEntry>, String> {
    let mut out = Vec::new();
    let read_dir = std::fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Collect ALL entries, dotfiles included: the browser's
        // "Show hidden files" toggle filters them GUI-side, the same way it
        // does for device listings. Filtering here made the toggle a no-op
        // in local mode.
        // DirEntry::metadata() does not follow symlinks; stat the target so
        // symlinked directories (e.g. /bin -> usr/bin) render as folders.
        let meta = std::fs::metadata(entry.path()).or_else(|_| entry.metadata());
        let Ok(meta) = meta else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        out.push(FsDirEntry {
            name,
            is_dir: meta.is_dir(),
            is_symlink: entry.file_type().map(|t| t.is_symlink()).unwrap_or(false),
            size: meta.len(),
            mode: meta.permissions().mode() & 0o7777,
            mtime,
        });
    }
    out.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// Sentinel "serial" for local-filesystem listings on the dir channel.
const LOCAL_DEVICE: &str = "__local__";

fn handle_browser_event(
    ev: BrowserEvent,
    handles: &std::rc::Rc<UiHandles>,
    dir_tx: &async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
    op_tx: &async_channel::Sender<(Option<String>, Result<String, String>)>,
    rt: tokio::runtime::Handle,
    window: adw::ApplicationWindow,
) {
    // Refresh the local filesystem listing after an operation.
    // (File operations chain their own refresh at the end of the task so
    // the listing only updates once the change actually landed.)
    fn refresh_local(
        dir_tx: &async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
        rt: &tokio::runtime::Handle,
        path: PathBuf,
    ) {
        let dir_tx = dir_tx.clone();
        rt.spawn_blocking(move || {
            let res = list_local_dir(&path);
            let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), path, res));
        });
    }

    match ev {
        BrowserEvent::DropFiles { from_dir, target_dir, files } => {
            let op_tx = op_tx.clone();
            if handles.browser.is_local_mode() {
                // Internal drags move; drops from other apps copy.
                let from_dir_for_task = from_dir.clone();
                let op_tx = op_tx.clone();
                rt.spawn_blocking(move || {
                    for src in files {
                        let Some(name) = src.file_name().and_then(|n| n.to_str()) else { continue };
                        let dst = target_dir.join(name);
                        if src == dst { continue; }
                        let internal = src.parent() == Some(from_dir_for_task.as_path());
                        let r = if internal {
                            std::fs::rename(&src, &dst)
                        } else {
                            std::fs::copy(&src, &dst).map(|_| ())
                        };
                        if let Err(e) = r {
                            let _ = op_tx.try_send((None, Err(format!("drop: {e}"))));
                        }
                    }
                });
                refresh_local(dir_tx, &rt, from_dir);
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else {
                let _ = op_tx.try_send((None, Err("No device connected — connect a device to drop files onto it.".into())));
                return;
            };
            let mount = handles.browser.fuse_mount();
            let dir_tx = dir_tx.clone();
            rt.spawn(async move {
                for src in files {
                    let Some(name) = src.file_name().and_then(|n| n.to_str()) else { continue };
                    let dst = target_dir.join(name);
                    match src.strip_prefix(mount.as_deref().unwrap_or("/nonexistent")) {
                        // Source lives on the device (dragged via the FUSE mount): move it.
                        Ok(rel) => {
                            // Dropped back into its own folder: nothing to do.
                            if src.parent() == Some(target_dir.as_path()) {
                                continue;
                            }
                            // The device proxy resolves paths from the device
                            // root, so the stripped relative path needs a
                            // leading '/' (e.g. "sdcard/Download/a" ->
                            // "/sdcard/Download/a").
                            let device_src = format!("/{}", rel.to_string_lossy());
                            if let Err(e) = rename(&device, &device_src, &dst.to_string_lossy()).await {
                                let _ = op_tx.try_send((None, Err(format!("move: {e}"))));
                            }
                        }
                        // External local file: push (copy) to the device.
                        Err(_) => {
                            if let Err(e) = enqueue_push(&device, &src.to_string_lossy(), &dst.to_string_lossy()).await {
                                let _ = op_tx.try_send((None, Err(format!("push: {e}"))));
                            }
                        }
                    }
                }
                let res = list_dir(&device, &from_dir.to_string_lossy()).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device, from_dir, res)).await;
            });
        }
        BrowserEvent::PauseTransfer => {
            // Toggle: first click pauses every active job, next click resumes.
            // Check for jobs FIRST: flipping the flag on an empty queue would
            // invert the polarity of the next real click.
            let ids = handles.active_jobs.lock().clone();
            if ids.is_empty() { return; }
            let resume = {
                let mut paused = handles.transfers_paused.lock();
                let resume = *paused;
                *paused = !resume;
                resume
            };
            rt.spawn(async move {
                for id in ids {
                    let r = if resume { resume_job(id).await } else { pause_job(id).await };
                    if let Err(e) = r {
                        tracing::warn!(id, error = %e, "pause/resume failed");
                    }
                }
            });
        }
        BrowserEvent::CancelTransfer => {
            let ids = handles.active_jobs.lock().split_off(0);
            if ids.is_empty() { return; }
            *handles.transfers_paused.lock() = false;
            rt.spawn(async move {
                for id in ids {
                    if let Err(e) = cancel_job(id).await {
                        tracing::warn!(id, error = %e, "cancel failed");
                    }
                }
            });
        }
        BrowserEvent::Up => {
            let current = handles.browser.current_path();
            if handles.browser.is_local_mode() {
                let parent = if current == PathBuf::from("/") {
                    PathBuf::from("/")
                } else {
                    current.parent().unwrap_or(Path::new("/")).to_path_buf()
                };
                refresh_local(dir_tx, &rt, parent);
                return;
            }
            let parent = if current == PathBuf::from("/") {
                PathBuf::from("/")
            } else {
                current.parent().unwrap_or(&PathBuf::from("/")).to_path_buf()
            };
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            handles.browser.set_loading(true);
            let dir_tx = dir_tx.clone();
            let path_str = parent.to_string_lossy().to_string();
            rt.spawn(async move {
                let res = list_dir(&device, &path_str).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device, parent, res)).await;
            });
        }
        BrowserEvent::Back | BrowserEvent::Forward => {}
        BrowserEvent::Navigate(path) => {
            if handles.browser.is_local_mode() {
                refresh_local(dir_tx, &rt, path);
                return;
            }
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            handles.browser.set_loading(true);
            let dir_tx = dir_tx.clone();
            let path_str = path.to_string_lossy().to_string();
            rt.spawn(async move {
                let res = list_dir(&device, &path_str).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device, path, res)).await;
            });
        }
        BrowserEvent::Refresh => {
            if handles.browser.is_local_mode() {
                refresh_local(dir_tx, &rt, handles.browser.current_path());
                return;
            }
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            let path = handles.browser.current_path();
            handles.browser.set_loading(true);
            let dir_tx = dir_tx.clone();
            let path_str = path.to_string_lossy().to_string();
            rt.spawn(async move {
                let res = list_dir(&device, &path_str).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device, path, res)).await;
            });
        }
        BrowserEvent::OpenDir(entry) => {
            if handles.browser.is_loading() { return; }
            if !entry.is_dir && !entry.is_symlink { return; }
            if handles.browser.is_local_mode() {
                let mut new_path = handles.browser.current_path();
                new_path.push(&entry.name);
                refresh_local(dir_tx, &rt, new_path);
                return;
            }
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            let mut new_path = handles.browser.current_path();
            new_path.push(&entry.name);
            handles.browser.set_loading(true);
            let dir_tx = dir_tx.clone();
            let path_str = new_path.to_string_lossy().to_string();
            rt.spawn(async move {
                let res = list_dir(&device, &path_str).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device, new_path, res)).await;
            });
        }
        BrowserEvent::Selected(_) => {}
        BrowserEvent::Upload => {
            if handles.browser.is_local_mode() {
                // Browsing local files: push a chosen file to the connected device.
                let Some(device) = handles.selected_device.lock().clone() else {
                    let _ = op_tx.try_send((None, Err("No device connected — connect a device in the sidebar to push files to it.".into())));
                    return;
                };
                let chooser = gtk4::FileChooserNative::builder()
                    .title("Pick a file to push to the device")
                    .modal(true)
                    .action(gtk4::FileChooserAction::Open)
                    .build();
                chooser.set_transient_for(Some(&window));
                let op_tx = op_tx.clone();
                chooser.connect_response(move |chooser, resp| {
                    if resp == gtk4::ResponseType::Accept {
                        if let Some(file) = chooser.file() {
                            if let Some(path) = file.path() {
                                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                                let local = path.to_string_lossy().to_string();
                                let device_path = format!("/sdcard/Download/{name}");
                                let device = device.clone();
                                let op_tx = op_tx.clone();
                                rt.spawn(async move {
                                    let r = enqueue_push(&device, &local, &device_path).await
                                        .map(|_| format!("Queued push of {name} to {device_path}"));
                                    let _ = op_tx.try_send((Some("ADB Push".into()), r.map_err(|e| e.to_string())));
                                });
                            }
                        }
                    }
                    chooser.destroy();
                });
                chooser.show();
                return;
            }
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            let current_dir = handles.browser.current_path();
            let chooser = gtk4::FileChooserNative::builder()
                .title("Pick file to upload")
                .modal(true)
                .action(gtk4::FileChooserAction::Open)
                .build();
            chooser.set_transient_for(Some(&window));

            let device_dir = current_dir.clone();
            let device_for_cb = device.clone();
            let dir_tx_for_cb = dir_tx.clone();
            let rt_for_cb = rt.clone();
            chooser.connect_response(move |chooser, resp| {
                if resp == gtk4::ResponseType::Accept {
                    if let Some(file) = chooser.file() {
                        if let Some(path) = file.path() {
                            let name = path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("upload")
                                .to_string();
                            let device_path = device_dir.join(&name).to_string_lossy().to_string();
                            let local = path.to_string_lossy().to_string();
                            let dir_tx = dir_tx_for_cb.clone();
                            let device_for_fetch = device_for_cb.clone();
                            let device_dir_for_fetch = device_dir.clone();
                            let device_dir_for_async = device_dir.clone();
                            rt_for_cb.spawn(async move {
                                let _ = enqueue_push(&device_for_fetch, &local, &device_path).await;
                                let refresh = list_dir(&device_for_fetch, &device_dir_for_async.to_string_lossy())
                                    .await
                                    .map_err(|e| e.to_string());
                                let _ = dir_tx.send((device_for_fetch.clone(), device_dir_for_fetch, refresh)).await;
                            });
                        }
                    }
                }
                chooser.destroy();
            });
            chooser.show();
        }
        BrowserEvent::Download(entries) => {
            if entries.is_empty() { return; }
            let files: Vec<FsDirEntry> = entries.into_iter().filter(|e| !e.is_dir).collect();
            if files.is_empty() { return; }

            if handles.browser.is_local_mode() {
                // Local browsing: copy the files to ~/Downloads.
                let Some(home) = dirs::home_dir() else { return };
                let curr = handles.browser.current_path();
                let dst_dir = home.join("Downloads");
                let op_tx = op_tx.clone();
                rt.spawn_blocking(move || {
                    let mut copied = 0;
                    let mut first_err = None;
                    for e in &files {
                        let src = curr.join(&e.name);
                        let dst = dst_dir.join(&e.name);
                        match std::fs::create_dir_all(&dst_dir)
                            .and_then(|_| std::fs::copy(&src, &dst).map(|_| ()))
                        {
                            Ok(_) => copied += 1,
                            Err(err) => { first_err = Some(format!("copy {}: {err}", e.name)); break; }
                        }
                    }
                    match first_err {
                        Some(err) => { let _ = op_tx.try_send((None, Err(err))); }
                        None => { let _ = op_tx.try_send((Some("Copy".into()), Ok(format!("Copied {copied} file(s) to {}", dst_dir.display())))); }
                    }
                });
                return;
            }

            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            let curr = handles.browser.current_path();

            if files.len() == 1 {
                // Single file: Save dialog with the name preset.
                let entry = files.into_iter().next().unwrap();
                let src = curr.join(&entry.name);
                let src_str = src.to_string_lossy().to_string();
                let chooser = gtk4::FileChooserNative::builder()
                    .title("Save file as")
                    .modal(true)
                    .action(gtk4::FileChooserAction::Save)
                    .build();
                chooser.set_current_name(&entry.name);
                if let Some(downloads) = dirs::download_dir() {
                    let _ = chooser.set_current_folder(Some(&gtk4::gio::File::for_path(downloads)));
                }
                chooser.set_transient_for(Some(&window));
                chooser.connect_response(move |chooser, resp| {
                    if resp == gtk4::ResponseType::Accept {
                        if let Some(file) = chooser.file() {
                            if let Some(path) = file.path() {
                                let local = path.to_string_lossy().to_string();
                                let device = device.clone();
                                let src_str = src_str.clone();
                                rt.spawn(async move {
                                    let _ = enqueue_pull(&device, &src_str, &local).await
                                        .map_err(|e| tracing::warn!(error=%e, "enqueue_pull failed"));
                                });
                            }
                        }
                    }
                    chooser.destroy();
                });
                chooser.show();
            } else {
                // Multiple files: pick a destination folder, pull them all.
                let chooser = gtk4::FileChooserNative::builder()
                    .title("Choose destination folder")
                    .modal(true)
                    .action(gtk4::FileChooserAction::SelectFolder)
                    .build();
                if let Some(downloads) = dirs::download_dir() {
                    let _ = chooser.set_current_folder(Some(&gtk4::gio::File::for_path(downloads)));
                }
                chooser.set_transient_for(Some(&window));
                chooser.connect_response(move |chooser, resp| {
                    if resp == gtk4::ResponseType::Accept {
                        if let Some(file) = chooser.file() {
                            if let Some(dest_dir) = file.path() {
                                for e in &files {
                                    let src = curr.join(&e.name).to_string_lossy().to_string();
                                    let local = dest_dir.join(&e.name).to_string_lossy().to_string();
                                    let device = device.clone();
                                    rt.spawn(async move {
                                        let _ = enqueue_pull(&device, &src, &local).await
                                            .map_err(|e| tracing::warn!(error=%e, "enqueue_pull failed"));
                                    });
                                }
                            }
                        }
                    }
                    chooser.destroy();
                });
                chooser.show();
            }
        }
        BrowserEvent::NewFolder(name) => {
            let curr = handles.browser.current_path();
            if handles.browser.is_local_mode() {
                let dir = curr.clone();
                let mut target = curr;
                target.push(&name);
                let op_tx = op_tx.clone();
                let dir_tx = dir_tx.clone();
                // Refresh AFTER the mkdir completes (success or failure);
                // refreshing in parallel races and usually wins, showing
                // stale contents.
                rt.spawn_blocking(move || {
                    if let Err(e) = std::fs::create_dir_all(&target) {
                        let _ = op_tx.try_send((None, Err(format!("mkdir: {e}"))));
                    }
                    let res = list_local_dir(&dir);
                    let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), dir, res));
                });
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else { return };
            let dir = handles.browser.current_path();
            let mut target = dir.clone();
            target.push(&name);
            let target_str = target.to_string_lossy().to_string();
            let dir_str = dir.to_string_lossy().to_string();
            let op_tx = op_tx.clone();
            let device_for_op = device.clone();
            let dir_tx = dir_tx.clone();
            rt.spawn(async move {
                if let Err(e) = mkdir(&device_for_op, &target_str).await {
                    let _ = op_tx.send((None, Err(format!("mkdir {target_str}: {e}")))).await;
                }
                let res = list_dir(&device_for_op, &dir_str).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device_for_op, dir, res)).await;
            });
        }
        BrowserEvent::Rename(entry, new_name) => {
            let curr = handles.browser.current_path();
            if handles.browser.is_local_mode() {
                let dir = curr.clone();
                let mut src = curr.clone(); src.push(&entry.name);
                let mut dst = curr.clone(); dst.push(&new_name);
                let op_tx = op_tx.clone();
                let dir_tx = dir_tx.clone();
                // Refresh AFTER the rename completes (success or failure).
                rt.spawn_blocking(move || {
                    if let Err(e) = std::fs::rename(&src, &dst) {
                        let _ = op_tx.try_send((None, Err(format!("rename: {e}"))));
                    }
                    let res = list_local_dir(&dir);
                    let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), dir, res));
                });
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else { return };
            let dir = handles.browser.current_path();
            let mut src = dir.clone(); src.push(&entry.name);
            let mut dst = dir.clone(); dst.push(&new_name);
            let src_str = src.to_string_lossy().to_string();
            let dst_str = dst.to_string_lossy().to_string();
            let op_tx = op_tx.clone();
            let device_for_op = device.clone();
            let dir_tx = dir_tx.clone();
            rt.spawn(async move {
                if let Err(e) = rename(&device_for_op, &src_str, &dst_str).await {
                    let _ = op_tx.send((None, Err(format!("rename {src_str}: {e}")))).await;
                }
                let res = list_dir(&device_for_op, &dir.to_string_lossy()).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device_for_op, dir, res)).await;
            });
        }
        BrowserEvent::Delete(entries) => {
            if entries.is_empty() { return; }
            let curr = handles.browser.current_path();
            if handles.browser.is_local_mode() {
                let op_tx = op_tx.clone();
                let dir_tx = dir_tx.clone();
                let dir = curr.clone();
                // Refresh AFTER the deletes complete (success or failure).
                rt.spawn_blocking(move || {
                    for e in entries {
                        let target = dir.join(&e.name);
                        let r = if e.is_dir {
                            std::fs::remove_dir_all(&target)
                        } else {
                            std::fs::remove_file(&target)
                        };
                        if let Err(err) = r {
                            let _ = op_tx.try_send((None, Err(format!("delete {}: {err}", e.name))));
                        }
                    }
                    let res = list_local_dir(&dir);
                    let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), dir, res));
                });
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else { return };
            let op_tx = op_tx.clone();
            let device_for_op = device.clone();
            let dir = curr.clone();
            let dir_tx = dir_tx.clone();
            rt.spawn(async move {
                for e in entries {
                    let target = dir.join(&e.name).to_string_lossy().to_string();
                    if let Err(err) = delete(&device_for_op, &target).await {
                        let _ = op_tx.try_send((None, Err(format!("delete {target}: {err}"))));
                    }
                }
                let res = list_dir(&device_for_op, &dir.to_string_lossy()).await.map_err(|e| e.to_string());
                let _ = dir_tx.send((device_for_op, dir, res)).await;
            });
        }
        BrowserEvent::OpenExternal(path) => {
            if handles.browser.is_local_mode() {
                if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                    let _ = op_tx.try_send((None, Err(format!("xdg-open {}: {e}", path.display()))));
                }
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else { return };
            let op_tx = op_tx.clone();
            rt.spawn(async move {
                match mountpoint_for(&device).await {
                    Ok(mp) => {
                        let local = format!("{}{}", mp.trim_end_matches('/'), path.display());
                        if let Err(e) = std::process::Command::new("xdg-open").arg(&local).spawn() {
                            let _ = op_tx.send((None, Err(format!("xdg-open {local}: {e}")))).await;
                        }
                    }
                    Err(e) => {
                        let _ = op_tx.send((None, Err(format!(
                            "The device is not mounted (file operations still work, but opening in the system file manager needs the FUSE mount): {e}"
                        )))).await;
                    }
                }
            });
        }
        BrowserEvent::InstallApk(entry) => {
            let curr = handles.browser.current_path();
            if handles.browser.is_local_mode() {
                // The APK is already local — install straight to a connected device.
                let Some(device) = handles.selected_device.lock().clone() else {
                    let _ = op_tx.try_send((None, Err("No device connected — connect a device to install the APK.".into())));
                    return;
                };
                let mut apk = curr.clone();
                apk.push(&entry.name);
                let apk_str = apk.to_string_lossy().to_string();
                let op_tx = op_tx.clone();
                rt.spawn(async move {
                    let out = tokio::process::Command::new("adb")
                        .args(["-s", &device, "install", "-r", &apk_str])
                        .output()
                        .await;
                    let r = match out {
                        Ok(o) if o.status.success() => Ok("APK installed".to_string()),
                        Ok(o) => Err(format!("adb install failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
                        Err(e) => Err(format!("adb install: {e}")),
                    };
                    let _ = op_tx.try_send((Some("Install APK".into()), r));
                });
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else { return };
            let mut target = handles.browser.current_path();
            target.push(&entry.name);
            let target_str = target.to_string_lossy().to_string();
            let op_tx = op_tx.clone();
            rt.spawn(async move {
                match mountpoint_for(&device).await {
                    Ok(mp) => {
                        let apk_local = format!("{}{}", mp.trim_end_matches('/'), target.display());
                        let out = tokio::process::Command::new("adb")
                            .args(["-s", &device, "install", "-r", &apk_local])
                            .output()
                            .await;
                        match out {
                            Ok(o) if o.status.success() => {
                                let _ = op_tx.send((Some("APK installed".into()), Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()))).await;
                            }
                            Ok(o) => {
                                let msg = String::from_utf8_lossy(&o.stderr);
                                let _ = op_tx.send((None, Err(format!("adb install failed: {}", msg.trim())))).await;
                            }
                            Err(e) => {
                                let _ = op_tx.send((None, Err(format!("adb install: {e}")))).await;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = op_tx.send((None, Err(format!(
                            "Installing an APK needs the FUSE mount to read the file locally: {e}"
                        )))).await;
                    }
                }
            });
        }
        BrowserEvent::OpenTerminal(path) => {
            let cmd = if handles.browser.is_local_mode() {
                format!("cd '{}' && exec $SHELL -l", path.display())
            } else {
                let device = match handles.selected_device.lock().clone() {
                    Some(d) => d,
                    None => return,
                };
                let path_str = path.to_string_lossy().to_string();
                format!("adb -s {} shell 'cd {} && exec $SHELL -l'", device, path_str)
            };
            let _ = std::process::Command::new("gnome-terminal")
                .args(["--", "bash", "-c", &format!("{}; exec bash", cmd)])
                .spawn()
                .or_else(|_| {
                    std::process::Command::new("x-terminal-emulator")
                        .args(["-e", &format!("bash -c \"{}; exec bash\"", cmd)])
                        .spawn()
                });
        }
    }
}

static MANAGER_PROXY: tokio::sync::OnceCell<adbshare_dbus_proxy::ManagerProxy<'static>> = tokio::sync::OnceCell::const_new();

async fn get_manager() -> anyhow::Result<&'static adbshare_dbus_proxy::ManagerProxy<'static>> {
    use zbus::{names::WellKnownName, Connection};
    MANAGER_PROXY.get_or_try_init(|| async {
        let conn = Connection::session().await?;
        let proxy = adbshare_dbus_proxy::ManagerProxy::builder(&conn)
            .destination(WellKnownName::try_from("org.adbshare.Manager")?)?
            .build()
            .await?;
        Ok(proxy)
    }).await
}

async fn list_devices() -> anyhow::Result<Vec<String>> {
    let proxy = get_manager().await?;
    Ok(proxy.list_devices().await?)
}

async fn device_info(serial: &str) -> anyhow::Result<String> {
    let proxy = get_manager().await?;
    Ok(proxy.device_info(serial).await?)
}

async fn pause_job(id: u64) -> anyhow::Result<bool> {
    let proxy = get_manager().await?;
    Ok(proxy.pause_job(id).await?)
}

async fn resume_job(id: u64) -> anyhow::Result<bool> {
    let proxy = get_manager().await?;
    Ok(proxy.resume_job(id).await?)
}

async fn cancel_job(id: u64) -> anyhow::Result<bool> {
    let proxy = get_manager().await?;
    Ok(proxy.cancel_job(id).await?)
}

async fn mkdir(device: &str, path: &str) -> anyhow::Result<()> {
    let proxy = get_manager().await?;
    Ok(proxy.mkdir(device, path).await?)
}

async fn rename(device: &str, src: &str, dst: &str) -> anyhow::Result<()> {
    let proxy = get_manager().await?;
    Ok(proxy.rename(device, src, dst).await?)
}

async fn delete(device: &str, path: &str) -> anyhow::Result<()> {
    let proxy = get_manager().await?;
    Ok(proxy.delete(device, path).await?)
}

async fn connect_wireless(address: &str) -> anyhow::Result<String> {
    let proxy = get_manager().await?;
    Ok(proxy.connect_wireless(address).await?)
}

async fn mountpoint_for(device: &str) -> anyhow::Result<String> {
    let proxy = get_manager().await?;
    Ok(proxy.mountpoint_for(device).await?)
}

fn show_error_dialog(window: &adw::ApplicationWindow, title: &str, msg: &str) {
    let dialog = adw::MessageDialog::builder()
        .heading(title)
        .body(msg)
        .modal(true)
        .build();
    dialog.add_response("ok", "OK");
    dialog.set_transient_for(Some(window));
    dialog.present();
}

/// "Connect ADB via IP" dialog: pair first (adb pair), then connect to the
/// device's adbd over TCP. The result is surfaced via `op_tx`.
fn show_connect_dialog(
    window: &adw::ApplicationWindow,
    op_tx: async_channel::Sender<(Option<String>, Result<String, String>)>,
    rt: tokio::runtime::Handle,
) {
    let dialog = gtk4::Dialog::builder()
        .title("Connect ADB via IP")
        .transient_for(window)
        .modal(true)
        .build();

    let content = dialog.content_area();
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_spacing(8);

    let hint = gtk4::Label::builder()
        .label("Pair first (adb pair <phone-ip>:<pair-port>), then connect:")
        .xalign(0.0)
        .wrap(true)
        .build();
    hint.add_css_class("dim-label");
    content.append(&hint);

    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("192.168.1.20:5555"));
    content.append(&entry);

    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
    let connect_btn = dialog.add_button("Connect", gtk4::ResponseType::Ok);
    connect_btn.add_css_class("suggested-action");

    let op_tx = op_tx.clone();
    dialog.connect_response(move |d, resp| {
        if resp == gtk4::ResponseType::Ok {
            let addr = entry.text().trim().to_string();
            if addr.is_empty() { return; }
            let op_tx = op_tx.clone();
            rt.spawn(async move {
                let res = connect_wireless(&addr)
                    .await
                    .map(|out| out.trim().to_string())
                    .map_err(|e| format!("adb connect {addr}: {e}"));
                let _ = op_tx.send((Some("ADB wireless connect".into()), res)).await;
            });
        }
        d.close();
    });

    dialog.present();
}

async fn list_dir(device: &str, path: &str) -> anyhow::Result<Vec<FsDirEntry>> {
    let proxy = get_manager().await?;
    let json = proxy.list_dir(device, path).await?;
    let entries: Vec<DirEntryDto> = serde_json::from_str(&json)?;
    Ok(entries.into_iter().map(FsDirEntry::from).collect())
}

async fn enqueue_push(device: &str, local: &str, device_path: &str) -> anyhow::Result<u64> {
    let proxy = get_manager().await?;
    Ok(proxy.enqueue_push(device, local, device_path).await?)
}

async fn enqueue_pull(device: &str, device_path: &str, local: &str) -> anyhow::Result<u64> {
    let proxy = get_manager().await?;
    Ok(proxy.enqueue_pull(device, device_path, local).await?)
}

async fn list_jobs() -> anyhow::Result<Vec<JobInfo>> {
    let proxy = get_manager().await?;
    let json = proxy.list_jobs().await?;
    let jobs: Vec<JobDto> = serde_json::from_str(&json)?;
    Ok(jobs.into_iter().map(JobInfo::from).collect())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DirEntryDto {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mode: u32,
    mtime: i64,
}

impl From<DirEntryDto> for FsDirEntry {
    fn from(d: DirEntryDto) -> Self {
        FsDirEntry {
            name: d.name,
            is_dir: d.is_dir,
            is_symlink: d.is_symlink,
            size: d.size,
            mode: d.mode,
            mtime: d.mtime,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct JobDto {
    id: u64,
    direction: String,
    source: String,
    destination: String,
    state: String,
    bytes_done: u64,
    bytes_total: u64,
    #[serde(default)]
    speed_bps: u64,
    #[serde(default)]
    eta_secs: u64,
}

impl From<JobDto> for JobInfo {
    fn from(j: JobDto) -> Self {
        let name = std::path::Path::new(&j.source)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&j.source)
            .to_string();
        JobInfo {
            id: j.id,
            direction: j.direction,
            name,
            state: j.state,
            bytes_done: j.bytes_done,
            bytes_total: j.bytes_total,
            speed_bps: j.speed_bps,
            eta_secs: j.eta_secs,
        }
    }
}