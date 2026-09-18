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
    async fn enqueue_push(
        &self,
        device: &str,
        local_path: &str,
        device_path: &str,
    ) -> zbus::Result<u64>;
    async fn enqueue_pull(
        &self,
        device: &str,
        device_path: &str,
        local_path: &str,
    ) -> zbus::Result<u64>;
    async fn list_jobs(&self) -> zbus::Result<String>;
    async fn pause_job(&self, id: u64) -> zbus::Result<bool>;
    async fn resume_job(&self, id: u64) -> zbus::Result<bool>;
    async fn cancel_job(&self, id: u64) -> zbus::Result<bool>;
    async fn retry_job(&self, id: u64) -> zbus::Result<bool>;
    async fn retry_failed(&self) -> zbus::Result<u64>;
    async fn enqueue_push_with_options(
        &self,
        device: &str,
        local_path: &str,
        device_path: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::Result<u64>;
    async fn enqueue_pull_with_options(
        &self,
        device: &str,
        device_path: &str,
        local_path: &str,
        overwrite: &str,
        verify: bool,
    ) -> zbus::Result<u64>;
    async fn mkdir(&self, device: &str, path: &str) -> zbus::Result<()>;
    async fn rename(&self, device: &str, src: &str, dst: &str) -> zbus::Result<()>;
    async fn copy_file(&self, device: &str, src: &str, dst: &str) -> zbus::Result<()>;
    async fn delete(&self, device: &str, path: &str) -> zbus::Result<()>;
    async fn connect_wireless(&self, address: &str) -> zbus::Result<String>;
    async fn mountpoint_for(&self, device: &str) -> zbus::Result<String>;
    async fn install_apk(&self, device: &str, path: &str) -> zbus::Result<String>;
}

mod adbshare_dbus_proxy {
    pub use super::ManagerProxy;
}

struct UiHandles {
    browser: FileBrowser,
    transfer: TransferView,
    selected_device: parking_lot::Mutex<Option<String>>,
    /// Cache of live device metadata from the daemon.
    devices: parking_lot::Mutex<Vec<crate::device_list::DeviceEntry>>,
    /// Ids of jobs currently Pending/Running (for banner Pause/Cancel).
    active_jobs: parking_lot::Mutex<Vec<u64>>,
    /// Whether the banner pause button currently means "resume".
    transfers_paused: parking_lot::Mutex<bool>,
    /// Ctrl+C snapshot: (from-local?, source dir, entry names). Paste (Ctrl+V)
    /// resolves it into push/pull/local-copy via the existing transfer paths.
    clipboard: parking_lot::Mutex<Option<ClipboardFiles>>,
}

/// In-app copy snapshot for Ctrl+C / Ctrl+V between the local and phone views.
#[derive(Debug, Clone)]
struct ClipboardFiles {
    /// True when the snapshot came from the local view; false = from a device.
    from_local: bool,
    /// Device serial when `from_local` is false.
    device: Option<String>,
    /// Directory that was browsed when copied (local FS path or /sdcard/...).
    from_dir: PathBuf,
    /// Copied entries (files only; dirs are skipped — daemon push/pull is
    /// file-based).
    entries: Vec<FsDirEntry>,
}

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
            css_provider.load_from_string(include_str!("style.css"));
            if let Some(display) = gdk4::Display::default() {
                gtk4::style_context_add_provider_for_display(
                    &display,
                    &css_provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_USER,
                );
            }

            let window = adw::ApplicationWindow::builder()
                .application(app)
                .title("ADBShare Files")
                .default_width(1000)
                .default_height(680)
                .width_request(360)
                .height_request(400)
                .build();
            window.add_css_class("background");

            // ═══════════════════════════════════════════════════════════
            //  LAYOUT — rebuilt from scratch for a modern, sleek look.
            //  Same structure: headerbar + sidebar/content body.
            // ═══════════════════════════════════════════════════════════
            let main_layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

            let browser = FileBrowser::new();
            browser.root.set_vexpand(true);
            browser.root.set_hexpand(true);

            // ── Header bar ──────────────────────────────────────────
            let header = adw::HeaderBar::new();
            header.add_css_class("sleek-headerbar");

            // 1. Left: Navigation Capsule  [≡ | ← | → | ↑]
            let nav_capsule = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            nav_capsule.add_css_class("pill-capsule");
            nav_capsule.set_valign(gtk4::Align::Center);

            let sidebar_toggle = gtk4::ToggleButton::builder()
                .tooltip_text("Toggle sidebar (F9)")
                .valign(gtk4::Align::Center)
                .active(true)
                .build();
            {
                let img = gtk4::Image::from_icon_name("sidebar-show-symbolic");
                img.set_pixel_size(16);
                sidebar_toggle.set_child(Some(&img));
            }
            sidebar_toggle.add_css_class("capsule-btn");
            sidebar_toggle.add_css_class("flat");
            nav_capsule.append(&sidebar_toggle);

            let sep0 = gtk4::Separator::new(gtk4::Orientation::Vertical);
            sep0.add_css_class("capsule-sep");
            nav_capsule.append(&sep0);

            for (i, btn) in [&browser.back_button, &browser.forward_button, &browser.up_button].iter().enumerate() {
                btn.add_css_class("capsule-btn");
                btn.add_css_class("flat");
                btn.set_valign(gtk4::Align::Center);
                nav_capsule.append(*btn);
                if i < 2 {
                    let s = gtk4::Separator::new(gtk4::Orientation::Vertical);
                    s.add_css_class("capsule-sep");
                    nav_capsule.append(&s);
                }
            }
            header.pack_start(&nav_capsule);

            // 2. Center: Omnibar location pill
            let omnibar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            omnibar.add_css_class("omnibar-pill");
            omnibar.set_valign(gtk4::Align::Center);
            omnibar.set_hexpand(true);

            let path_icon = gtk4::Image::from_icon_name("folder-symbolic");
            path_icon.add_css_class("omnibar-leading-icon");
            path_icon.set_valign(gtk4::Align::Center);
            path_icon.set_pixel_size(14);
            omnibar.append(&path_icon);

            browser.path_stack.set_hexpand(true);
            browser.path_stack.set_valign(gtk4::Align::Center);
            browser.breadcrumb_container.set_halign(gtk4::Align::Start);
            omnibar.append(&browser.path_stack);

            browser.path_edit_toggle.add_css_class("omnibar-sub-btn");
            browser.path_edit_toggle.add_css_class("flat");
            browser.path_edit_toggle.set_valign(gtk4::Align::Center);
            omnibar.append(&browser.path_edit_toggle);

            header.set_title_widget(Some(&omnibar));

            // Transfer popover (full queue)
            let transfer = TransferView::new();
            let trans_pop = gtk4::Popover::new();
            trans_pop.set_size_request(460, 360);
            let trans_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            let trans_hdr = gtk4::Label::builder()
                .label("Transfers")
                .xalign(0.0)
                .margin_start(14)
                .margin_top(12)
                .margin_bottom(4)
                .build();
            trans_hdr.add_css_class("heading");
            trans_box.append(&trans_hdr);
            let trans_sub = gtk4::Label::builder()
                .label("Copies between this computer and the phone.")
                .xalign(0.0)
                .margin_start(14)
                .margin_bottom(8)
                .wrap(true)
                .build();
            trans_sub.add_css_class("dim-label");
            trans_box.append(&trans_sub);
            let policy_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            policy_row.set_margin_start(14);
            policy_row.set_margin_end(14);
            policy_row.set_margin_bottom(4);
            let policy_label = gtk4::Label::builder()
                .label("If destination exists:")
                .xalign(0.0)
                .hexpand(true)
                .build();
            policy_label.add_css_class("dim-label");
            policy_row.append(&policy_label);
            let policy_combo = gtk4::ComboBoxText::new();
            policy_combo.append_text("Skip");
            policy_combo.append_text("Replace");
            policy_combo.append_text("Keep both");
            policy_combo.set_active(Some(0));
            policy_row.append(&policy_combo);
            trans_box.append(&policy_row);
            let verify_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            verify_row.set_margin_start(14);
            verify_row.set_margin_end(14);
            verify_row.set_margin_bottom(4);
            let verify_check = gtk4::CheckButton::with_label("Verify transfers (SHA-256)");
            verify_row.append(&verify_check);
            let retry_btn = gtk4::Button::with_label("Retry failed");
            retry_btn.set_halign(gtk4::Align::End);
            retry_btn.set_hexpand(true);
            verify_row.append(&retry_btn);
            trans_box.append(&verify_row);
            {
                let combo_changed = policy_combo.clone();
                let check_changed = verify_check.clone();
                policy_combo.connect_changed(move |_| {
                    let policy = match combo_changed.active_text().as_deref() {
                        Some("Replace") => "replace",
                        Some("Keep both") => "keep-both",
                        _ => "skip",
                    };
                    set_transfer_policy(policy, check_changed.is_active());
                });
                let combo_toggled = policy_combo.clone();
                let check_toggled = verify_check.clone();
                verify_check.connect_toggled(move |_| {
                    let policy = match combo_toggled.active_text().as_deref() {
                        Some("Replace") => "replace",
                        Some("Keep both") => "keep-both",
                        _ => "skip",
                    };
                    set_transfer_policy(policy, check_toggled.is_active());
                });
            }
            {
                let rt_retry = rt_handle.clone();
                retry_btn.connect_clicked(move |_| {
                    rt_retry.spawn(async move {
                        if let Err(e) = retry_failed().await {
                            tracing::warn!(error = %e, "retry_failed failed");
                        }
                    });
                });
            }
            transfer.attach(&trans_box);
            trans_pop.set_child(Some(&trans_box));

            // 3. Right: Operations + View + Transfers + Kebab
            let header_end = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            header_end.set_valign(gtk4::Align::Center);

            // Helper: creates an icon image at a uniform size for header buttons.
            let hdr_icon = |name: &str| -> gtk4::Image {
                let img = gtk4::Image::from_icon_name(name);
                img.set_pixel_size(16);
                img
            };

            // Ops capsule: [new folder | upload | download]
            let ops_capsule = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            ops_capsule.add_css_class("pill-capsule");
            ops_capsule.set_valign(gtk4::Align::Center);

            browser.new_folder_button.add_css_class("capsule-btn");
            browser.new_folder_button.add_css_class("flat");
            browser.new_folder_button.set_valign(gtk4::Align::Center);
            browser.new_folder_button.set_has_frame(false);
            browser.new_folder_button.set_child(Some(&hdr_icon("folder-new-symbolic")));
            browser.new_folder_button.set_tooltip_text(Some("New folder"));
            ops_capsule.append(&browser.new_folder_button);

            let sep_o1 = gtk4::Separator::new(gtk4::Orientation::Vertical);
            sep_o1.add_css_class("capsule-sep");
            ops_capsule.append(&sep_o1);

            browser.upload_button.add_css_class("capsule-btn");
            browser.upload_button.add_css_class("flat");
            browser.upload_button.set_valign(gtk4::Align::Center);
            browser.upload_button.set_has_frame(false);
            browser.upload_button.set_child(Some(&hdr_icon("document-send-symbolic")));
            browser.upload_button.set_tooltip_text(Some("Send to phone (Ctrl+U)"));
            ops_capsule.append(&browser.upload_button);

            let sep_o2 = gtk4::Separator::new(gtk4::Orientation::Vertical);
            sep_o2.add_css_class("capsule-sep");
            ops_capsule.append(&sep_o2);

            browser.download_button.add_css_class("capsule-btn");
            browser.download_button.add_css_class("flat");
            browser.download_button.set_valign(gtk4::Align::Center);
            browser.download_button.set_has_frame(false);
            browser.download_button.set_child(Some(&hdr_icon("folder-download-symbolic")));
            browser.download_button.set_tooltip_text(Some("Save to computer (Ctrl+Shift+C)"));
            ops_capsule.append(&browser.download_button);

            header_end.append(&ops_capsule);

            // View capsule: [grid/list | search]
            let view_capsule = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            view_capsule.add_css_class("pill-capsule");
            view_capsule.set_valign(gtk4::Align::Center);

            let view_toggle = gtk4::Button::new();
            let view_icon = hdr_icon("view-list-symbolic");
            view_toggle.set_child(Some(&view_icon));
            view_toggle.add_css_class("capsule-btn");
            view_toggle.add_css_class("flat");
            view_toggle.set_tooltip_text(Some("Switch to list view"));
            view_toggle.set_valign(gtk4::Align::Center);
            {
                let browser_vt = browser.clone();
                let vt = view_toggle.clone();
                let vi = view_icon.clone();
                view_toggle.connect_clicked(move |_| {
                    let new_mode = if browser_vt.view_mode() == ViewMode::Grid {
                        ViewMode::List
                    } else {
                        ViewMode::Grid
                    };
                    browser_vt.set_view_mode(new_mode);
                    if new_mode == ViewMode::Grid {
                        vi.set_icon_name(Some("view-list-symbolic"));
                        vt.set_tooltip_text(Some("Switch to list view"));
                    } else {
                        vi.set_icon_name(Some("view-grid-symbolic"));
                        vt.set_tooltip_text(Some("Switch to grid view"));
                    }
                });
            }
            view_capsule.append(&view_toggle);

            let sep_v = gtk4::Separator::new(gtk4::Orientation::Vertical);
            sep_v.add_css_class("capsule-sep");
            view_capsule.append(&sep_v);

            browser.search_button.add_css_class("capsule-btn");
            browser.search_button.add_css_class("flat");
            browser.search_button.set_valign(gtk4::Align::Center);
            view_capsule.append(&browser.search_button);

            header_end.append(&view_capsule);

            // Transfers pill badge — same 34px via CSS, uniform icon
            let transfers_btn = gtk4::MenuButton::new();
            transfers_btn.add_css_class("pill-capsule-btn");
            transfers_btn.add_css_class("flat");
            let trans_box_inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 5);
            trans_box_inner.set_valign(gtk4::Align::Center);
            let trans_dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            trans_dot.add_css_class("transfer-indicator-dot");
            trans_dot.set_valign(gtk4::Align::Center);
            trans_box_inner.append(&trans_dot);
            trans_box_inner.append(&hdr_icon("emblem-synchronizing-symbolic"));
            transfers_btn.set_child(Some(&trans_box_inner));
            transfers_btn.set_tooltip_text(Some("Transfers (idle)"));
            transfers_btn.set_valign(gtk4::Align::Center);
            transfers_btn.set_popover(Some(&trans_pop));
            header_end.append(&transfers_btn);

            // Kebab overflow menu — same 34px via CSS, uniform icon
            let kebab = gtk4::MenuButton::new();
            let kebab_pop = gtk4::Popover::new();
            kebab.set_child(Some(&hdr_icon("view-more-symbolic")));
            kebab.add_css_class("pill-capsule-btn");
            kebab.add_css_class("flat");
            kebab.set_tooltip_text(Some("More options"));
            kebab.set_valign(gtk4::Align::Center);
            let kebab_menu = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
            kebab_menu.set_margin_top(6);
            kebab_menu.set_margin_bottom(6);
            kebab_menu.set_margin_start(4);
            kebab_menu.set_margin_end(4);
            kebab_menu.set_size_request(240, -1);
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
            let install_apk_item = menu_item("system-software-install-symbolic", "Install APK…");
            {
                let browser_apk = browser.clone();
                let kp = kebab_pop.clone();
                install_apk_item.connect_clicked(move |_| {
                    kp.popdown();
                    browser_apk.emit(crate::file_browser::BrowserEvent::SideloadApkPrompt);
                });
            }
            kebab_menu.append(&install_apk_item);
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
                .active(browser.show_hidden())
                .build();
            {
                let browser_h = browser.clone();
                let kp = kebab_pop.clone();
                hidden_check.connect_toggled(move |btn| {
                    kp.popdown();
                    if btn.is_active() != browser_h.show_hidden() {
                        browser_h.toggle_show_hidden();
                    }
                });
            }
            {
                let hc = hidden_check.clone();
                let browser_h = browser.clone();
                kebab_pop.connect_notify_local(Some("visible"), move |pop, _| {
                    if pop.is_visible() {
                        hc.set_active(browser_h.show_hidden());
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

            // Search row: hidden until search toggle is on
            browser.search_entry.set_placeholder_text(Some("Search this folder…"));
            browser.search_entry.set_hexpand(true);
            let search_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            search_row.set_margin_start(14);
            search_row.set_margin_end(14);
            search_row.set_margin_top(4);
            search_row.set_margin_bottom(4);
            search_row.append(&browser.search_entry);
            browser.search_bar.set_child(Some(&search_row));

            // ── Body: responsive overlay split view ─────────────────
            let split_view = adw::OverlaySplitView::new();
            split_view.set_vexpand(true);
            split_view.set_hexpand(true);
            split_view.set_min_sidebar_width(200.0);
            split_view.set_max_sidebar_width(300.0);
            split_view.set_sidebar_width_fraction(0.22);
            split_view.set_enable_show_gesture(true);
            split_view.set_enable_hide_gesture(true);

            let sidebar_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            sidebar_box.add_css_class("navigation-sidebar");

            let device_list = std::rc::Rc::new(DeviceList::new());
            device_list.populate_defaults();
            device_list.attach(&sidebar_box);

            let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            content_box.set_vexpand(true);
            content_box.set_hexpand(true);
            content_box.append(&browser.search_bar);
            content_box.append(&browser.root);

            split_view.set_sidebar(Some(&sidebar_box));
            split_view.set_content(Some(&content_box));
            main_layout.append(&split_view);

            let sv_toggle = split_view.clone();
            sidebar_toggle.connect_toggled(move |btn| {
                sv_toggle.set_show_sidebar(btn.is_active());
            });
            let btn_toggle = sidebar_toggle.clone();
            split_view.connect_show_sidebar_notify(move |sv| {
                btn_toggle.set_active(sv.shows_sidebar());
            });

            // Breakpoint: collapse sidebar on narrow viewports
            let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
                adw::BreakpointConditionLengthType::MaxWidth,
                760.0,
                adw::LengthUnit::Sp,
            ));
            breakpoint.add_setter(&split_view, "collapsed", Some(&true.into()));
            window.add_breakpoint(breakpoint);

            // F9 shortcut toggles the sidebar
            let key_ctrl = gtk4::EventControllerKey::new();
            let sv_key = split_view.clone();
            key_ctrl.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == gdk4::Key::F9 {
                    sv_key.set_show_sidebar(!sv_key.shows_sidebar());
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            window.add_controller(key_ctrl);

            window.set_content(Some(&main_layout));

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
                selected_device: parking_lot::Mutex::new(None),
                devices: parking_lot::Mutex::new(Vec::new()),
                active_jobs: parking_lot::Mutex::new(Vec::new()),
                transfers_paused: parking_lot::Mutex::new(false),
                clipboard: parking_lot::Mutex::new(None),
            });

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
                        let _ = std::fs::create_dir_all(&path);
                    }
                    if !path.is_dir() {
                        let _ = op_tx_sidebar.try_send((None, Err(format!("{} does not exist", path.display()))));
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
                    handles_sidebar.browser.set_device(Some(&device));
                    let cached = handles_sidebar
                        .devices
                        .lock()
                        .iter()
                        .find(|d| d.serial == device)
                        .cloned();
                    if let Some(ref entry) = cached {
                        handles_sidebar.browser.set_device_info(entry);
                    }
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

            // --- Drain device list -> sidebar + notifications ---
            let dev_list_drain = device_list.clone();
            let handles_dev_drain = handles.clone();
            let dir_tx_dev_drain = dir_tx.clone();
            let info_tx_dev_drain = info_tx.clone();
            let mp_tx_dev_drain = mp_tx.clone();
            let rt_dev_drain = rt_handle.clone();
            let app_notify = app.clone();
            // ponytail: previous serials live in this drain; global store
            // only if second consumer needs them.
            glib::spawn_future_local(async move {
                let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut first_poll = true;
                while let Ok(result) = devices_rx.recv().await {
                    match result {
                        Ok(devices) => {
                            // Diff before overwrite: added/removed drive notifications.
                            let current: std::collections::HashSet<String> =
                                devices.iter().map(|d| d.serial.clone()).collect();
                            if !first_poll {
                                for serial in current.difference(&known) {
                                    let name = devices.iter().find(|d| &d.serial == serial)
                                        .map(|d| d.display_name().to_string())
                                        .unwrap_or_else(|| serial.clone());
                                    let note = gtk4::gio::Notification::new(&format!("{} connected", name));
                                    note.set_body(Some("Tap to browse files"));
                                    app_notify.send_notification(Some(&format!("device-{}", serial)), &note);
                                }
                                for serial in known.difference(&current) {
                                    app_notify.withdraw_notification(&format!("device-{}", serial));
                                    let note = gtk4::gio::Notification::new("Device disconnected");
                                    note.set_body(Some(serial.as_str()));
                                    app_notify.send_notification(Some(&format!("device-gone-{}", serial)), &note);
                                }
                            }
                            known = current;
                            first_poll = false;
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
                            // Auto-select the first real device if none selected and not browsing local files.
                            if selected.is_none() && !handles_dev_drain.browser.is_local_mode() {
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

            // --- Drain jobs -> status bar, header count, full list ---
            // The status bar owns progress.
            let handles_jobs = handles.clone();
            let browser_drain = handles.browser.clone();
            let btn_drain = transfers_btn.clone();
            let dot_drain = trans_dot.clone();
            glib::spawn_future_local(async move {
                while let Ok(result) = jobs_rx.recv().await {
                    match result {
                        Ok(jobs) => {
                            // Paused jobs count as active: they must stay
                            // visible and resumable.
                            let active: Vec<_> = jobs.iter().filter(|j| {
                                j.state == "Running" || j.state == "Pending" || j.state == "Paused"
                            }).collect();
                            *handles_jobs.active_jobs.lock() = active.iter().map(|j| j.id).collect();
                            if active.is_empty() {
                                dot_drain.remove_css_class("active");
                                btn_drain.set_tooltip_text(Some("Transfers (idle)"));
                            } else {
                                dot_drain.add_css_class("active");
                                if active.len() == 1 {
                                    btn_drain.set_tooltip_text(Some("1 active transfer"));
                                } else {
                                    btn_drain.set_tooltip_text(Some(&format!("{} active transfers", active.len())));
                                }
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
                            browser_drain.status_pause_btn.set_icon_name(icon);
                            browser_drain.status_pause_btn.set_tooltip_text(Some(tip));
                            handles_jobs.transfer.update_jobs(jobs);
                        }
                        Err(e) => tracing::warn!(error=%e, "list_jobs failed"),
                    }
                }
            });

            if let Ok(ms) = std::env::var("ADB_GUI_AUTOCLOSE_MS") {
                if let Ok(ms) = ms.parse::<u64>() {
                    let app = app.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
                        app.quit();
                    });
                }
            }

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
            transport: if self.transport == "wifi" {
                "wifi"
            } else {
                "usb"
            },
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
    let cached = handles
        .devices
        .lock()
        .iter()
        .find(|d| d.serial == serial)
        .cloned();
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
            let _ = mp_tx
                .send(mountpoint_for(&serial_m).await.map_err(|e| e.to_string()))
                .await;
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
        let res = list_dir(&serial_c, "/sdcard/Download")
            .await
            .map_err(|e| e.to_string());
        let _ = dir_tx
            .send((serial_c, PathBuf::from("/sdcard/Download"), res))
            .await;
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
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

/// Sentinel "serial" for local-filesystem listings on the dir channel.
const LOCAL_DEVICE: &str = "__local__";

fn paste_clipboard(
    handles: &std::rc::Rc<UiHandles>,
    rt: &tokio::runtime::Handle,
    dir_tx: &async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
    op_tx: &async_channel::Sender<(Option<String>, Result<String, String>)>,
) {
    fn refresh_after(
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
    let Some(snap) = handles.clipboard.lock().clone() else {
        paste_gdk_text(handles, rt, dir_tx, op_tx);
        return;
    };
    let target_dir = handles.browser.current_path();
    let to_local = handles.browser.is_local_mode();
    match (snap.from_local, to_local) {
        (true, true) => {
            let from = snap.from_dir.clone();
            let files = snap.entries.clone();
            let op_tx = op_tx.clone();
            let dir_tx = dir_tx.clone();
            let rt2 = rt.clone();
            rt.spawn_blocking(move || {
                for e in &files {
                    let src = from.join(&e.name);
                    let dst = target_dir.join(&e.name);
                    if src == dst {
                        continue;
                    }
                    if let Err(err) = std::fs::copy(&src, &dst) {
                        let _ = op_tx.try_send((None, Err(format!("paste: {err}"))));
                    }
                }
                refresh_after(&dir_tx, &rt2, target_dir);
            });
        }
        (false, false) => {
            let Some(device) = handles.selected_device.lock().clone() else {
                let _ = op_tx.try_send((
                    None,
                    Err("No device connected — connect a device to paste.".into()),
                ));
                return;
            };
            if snap.device.as_deref() != Some(device.as_str()) {
                let _ = op_tx.try_send((
                    None,
                    Err("Pasting between two phones is not supported yet.".into()),
                ));
                return;
            }
            let from = snap.from_dir.clone();
            let files = snap.entries.clone();
            let dir_tx = dir_tx.clone();
            let op_tx = op_tx.clone();
            rt.clone().spawn(async move {
                for e in &files {
                    let src = from.join(&e.name);
                    let dst = target_dir.join(&e.name);
                    if src == dst {
                        continue;
                    }
                    if let Err(err) =
                        copy_file(&device, &src.to_string_lossy(), &dst.to_string_lossy()).await
                    {
                        let _ = op_tx.try_send((None, Err(format!("copy {}: {err}", e.name))));
                    }
                }
                let res = list_dir(&device, &target_dir.to_string_lossy())
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device, target_dir, res)).await;
            });
        }
        (true, false) => {
            let Some(device) = handles.selected_device.lock().clone() else {
                let _ = op_tx.try_send((
                    None,
                    Err("No device connected — connect a device to paste.".into()),
                ));
                return;
            };
            let from = snap.from_dir.clone();
            let files: Vec<PathBuf> = snap
                .entries
                .iter()
                .map(|e| snap.from_dir.join(&e.name))
                .collect();
            let dir_tx = dir_tx.clone();
            let op_tx = op_tx.clone();
            rt.clone().spawn(async move {
                for src in &files {
                    let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    let dst = target_dir.join(name);
                    if let Err(e) =
                        enqueue_push(&device, &src.to_string_lossy(), &dst.to_string_lossy()).await
                    {
                        let _ = op_tx.try_send((None, Err(format!("push: {e}"))));
                    }
                }
                let res = list_dir(&device, &from.to_string_lossy())
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device, from, res)).await;
            });
        }
        (false, true) => {
            let Some(device) = snap.device.clone() else {
                return;
            };
            let target_dir = handles.browser.current_path();
            let dir_tx = dir_tx.clone();
            let op_tx = op_tx.clone();
            rt.clone().spawn(async move {
                for e in &snap.entries {
                    let src = snap.from_dir.join(&e.name).to_string_lossy().to_string();
                    let local = target_dir.join(&e.name).to_string_lossy().to_string();
                    if let Err(err) = enqueue_pull(&device, &src, &local).await {
                        let _ = op_tx.try_send((None, Err(format!("pull: {err}"))));
                    }
                }
                let res = list_dir(&device, &snap.from_dir.to_string_lossy())
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device, snap.from_dir.clone(), res)).await;
            });
        }
    }
}

/// Paste paths copied from another app (GDK clipboard text / file URIs).
/// Local paths pasted onto the phone push via `enqueue_push`; pasted while
/// browsing local files they copy with `std::fs::copy`.
fn paste_gdk_text(
    handles: &std::rc::Rc<UiHandles>,
    rt: &tokio::runtime::Handle,
    dir_tx: &async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
    op_tx: &async_channel::Sender<(Option<String>, Result<String, String>)>,
) {
    let Some(display) = gdk4::Display::default() else {
        return;
    };
    let clipboard = display.clipboard();
    let handles = std::rc::Rc::clone(handles);
    let dir_tx = dir_tx.clone();
    let op_tx = op_tx.clone();
    let rt = rt.clone();
    glib::spawn_future_local(async move {
        let text = match clipboard.read_text_future().await {
            Ok(Some(t)) => t.to_string(),
            Ok(None) | Err(_) => return,
        };
        let paths: Vec<PathBuf> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .filter_map(|l| {
                let l = l.strip_prefix("file://").unwrap_or(l);
                let decoded = url_decode(l);
                let p = PathBuf::from(decoded);
                if p.is_absolute() { Some(p) } else { None }
            })
            .collect();
        if paths.is_empty() {
            return;
        }
        paste_external_paths(&handles, &rt, &dir_tx, &op_tx, paths);
    });
}

/// Paste local paths from an external app into the browsed directory.
fn paste_external_paths(
    handles: &std::rc::Rc<UiHandles>,
    rt: &tokio::runtime::Handle,
    dir_tx: &async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
    op_tx: &async_channel::Sender<(Option<String>, Result<String, String>)>,
    paths: Vec<PathBuf>,
) {
    let target_dir = handles.browser.current_path();
    if handles.browser.is_local_mode() {
        let op_tx = op_tx.clone();
        let dir_tx = dir_tx.clone();
        let rt2 = rt.clone();
        rt.spawn_blocking(move || {
            for src in &paths {
                let Some(name) = src.file_name() else {
                    continue;
                };
                let dst = target_dir.join(name);
                if src == &dst {
                    continue;
                }
                if let Err(e) = std::fs::copy(src, &dst) {
                    let _ = op_tx.try_send((None, Err(format!("paste: {e}"))));
                }
            }
            let dir_tx = dir_tx.clone();
            rt2.spawn_blocking(move || {
                let res = list_local_dir(&target_dir);
                let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), target_dir, res));
            });
        });
        return;
    }
    let Some(device) = handles.selected_device.lock().clone() else {
        let _ = op_tx.try_send((
            None,
            Err("No device connected — connect a device to paste.".into()),
        ));
        return;
    };
    let dir_tx = dir_tx.clone();
    let op_tx = op_tx.clone();
    rt.clone().spawn(async move {
        for src in &paths {
            let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let dst = target_dir.join(name);
            if let Err(e) =
                enqueue_push(&device, &src.to_string_lossy(), &dst.to_string_lossy()).await
            {
                let _ = op_tx.try_send((None, Err(format!("push: {e}"))));
            }
        }
        let res = list_dir(&device, &target_dir.to_string_lossy())
            .await
            .map_err(|e| e.to_string());
        let _ = dir_tx.send((device, target_dir, res)).await;
    });
}

/// Percent-decode a `file://` URI path (UTF-8, lossy; `+` stays literal).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

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

    fn push_dropped_files_to_device(
        device: String,
        files: Vec<PathBuf>,
        from_dir: PathBuf,
        target_dir: PathBuf,
        mount: Option<String>,
        dir_tx: async_channel::Sender<(String, PathBuf, Result<Vec<FsDirEntry>, String>)>,
        op_tx: async_channel::Sender<(Option<String>, Result<String, String>)>,
        rt: tokio::runtime::Handle,
    ) {
        rt.spawn(async move {
            for src in files {
                let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let dst = target_dir.join(name);
                match src.strip_prefix(mount.as_deref().unwrap_or("/nonexistent")) {
                    Ok(rel) => {
                        if src.parent() == Some(target_dir.as_path()) {
                            continue;
                        }
                        let device_src = format!("/{}", rel.to_string_lossy());
                        if let Err(e) =
                            rename(&device, &device_src, &dst.to_string_lossy()).await
                        {
                            let _ = op_tx.try_send((None, Err(format!("move: {e}"))));
                        }
                    }
                    Err(_) => {
                        if let Err(e) = enqueue_push(
                            &device,
                            &src.to_string_lossy(),
                            &dst.to_string_lossy(),
                        )
                        .await
                        {
                            let _ = op_tx.try_send((None, Err(format!("push: {e}"))));
                        }
                    }
                }
            }
            let res = list_dir(&device, &from_dir.to_string_lossy())
                .await
                .map_err(|e| e.to_string());
            let _ = dir_tx.send((device, from_dir, res)).await;
        });
    }

    match ev {
        BrowserEvent::DropFiles {
            from_dir,
            target_dir,
            files,
        } => {
            let op_tx = op_tx.clone();
            if handles.browser.is_local_mode() {
                // Internal drags move; drops from other apps copy.
                let from_dir_for_task = from_dir.clone();
                let op_tx = op_tx.clone();
                rt.spawn_blocking(move || {
                    for src in files {
                        let Some(name) = src.file_name().and_then(|n| n.to_str()) else {
                            continue;
                        };
                        let dst = target_dir.join(name);
                        if src == dst {
                            continue;
                        }
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
                let _ = op_tx.try_send((
                    None,
                    Err("No device connected — connect a device to drop files onto it.".into()),
                ));
                return;
            };

            let apk_files: Vec<PathBuf> = files
                .iter()
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("apk"))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            let mount = handles.browser.fuse_mount();

            if !apk_files.is_empty() {
                let apk_names = apk_files
                    .iter()
                    .filter_map(|f| f.file_name().and_then(|n| n.to_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let dialog = adw::MessageDialog::builder()
                    .heading("Install or Copy APK?")
                    .body(format!(
                        "You dropped '{apk_names}'. Would you like to install it to the connected phone or copy it to this folder?"
                    ))
                    .modal(true)
                    .transient_for(&window)
                    .build();
                dialog.add_response("install", "Install APK");
                dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
                dialog.add_response("copy", "Copy to Folder");
                dialog.add_response("cancel", "Cancel");

                let handles_d = handles.clone();
                let op_tx_d = op_tx.clone();
                let rt_d = rt.clone();
                let dev_d = device.clone();
                let files_d = files.clone();
                let from_dir_d = from_dir.clone();
                let target_dir_d = target_dir.clone();
                let mount_d = mount.clone();
                let dir_tx_d = dir_tx.clone();

                dialog.connect_response(None, move |_, resp| {
                    if resp == "install" {
                        for apk in apk_files.clone() {
                            let path_str = apk.to_string_lossy().to_string();
                            let name = apk
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("app.apk")
                                .to_string();
                            run_apk_install(dev_d.clone(), path_str, name, &handles_d, &op_tx_d, &rt_d);
                        }
                    } else if resp == "copy" {
                        push_dropped_files_to_device(
                            dev_d.clone(),
                            files_d.clone(),
                            from_dir_d.clone(),
                            target_dir_d.clone(),
                            mount_d.clone(),
                            dir_tx_d.clone(),
                            op_tx_d.clone(),
                            rt_d.clone(),
                        );
                    }
                });
                dialog.present();
            } else {
                push_dropped_files_to_device(
                    device,
                    files,
                    from_dir,
                    target_dir,
                    mount,
                    dir_tx.clone(),
                    op_tx.clone(),
                    rt.clone(),
                );
            }
        }
        BrowserEvent::CopyFiles(entries) => {
            let from_local = handles.browser.is_local_mode();
            let from_dir = handles.browser.current_path();
            let files: Vec<FsDirEntry> = entries.into_iter().filter(|e| !e.is_dir).collect();
            if files.is_empty() {
                return;
            }
            let device = handles.selected_device.lock().clone();
            // GDK mirror: plain paths as text so other apps see the copy too;
            // the in-app snapshot below is what Ctrl+V resolves into transfers.
            if let Some(display) = gdk4::Display::default() {
                let text = files
                    .iter()
                    .map(|e| from_dir.join(&e.name).to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                display.clipboard().set_text(&text);
            }
            *handles.clipboard.lock() = Some(ClipboardFiles {
                from_local,
                device,
                from_dir,
                entries: files,
            });
        }
        BrowserEvent::Paste => {
            paste_clipboard(handles, &rt, dir_tx, op_tx);
        }
        BrowserEvent::PauseTransfer => {
            // Toggle: first click pauses every active job, next click resumes.
            // Check for jobs FIRST: flipping the flag on an empty queue would
            // invert the polarity of the next real click.
            let ids = handles.active_jobs.lock().clone();
            if ids.is_empty() {
                return;
            }
            let resume = {
                let mut paused = handles.transfers_paused.lock();
                let resume = *paused;
                *paused = !resume;
                resume
            };
            rt.spawn(async move {
                for id in ids {
                    let r = if resume {
                        resume_job(id).await
                    } else {
                        pause_job(id).await
                    };
                    if let Err(e) = r {
                        tracing::warn!(id, error = %e, "pause/resume failed");
                    }
                }
            });
        }
        BrowserEvent::CancelTransfer => {
            let ids = handles.active_jobs.lock().split_off(0);
            if ids.is_empty() {
                return;
            }
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
                current
                    .parent()
                    .unwrap_or(&PathBuf::from("/"))
                    .to_path_buf()
            };
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => return,
            };
            handles.browser.set_loading(true);
            let dir_tx = dir_tx.clone();
            let path_str = parent.to_string_lossy().to_string();
            rt.spawn(async move {
                let res = list_dir(&device, &path_str)
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device, parent, res)).await;
            });
        }
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
                let res = list_dir(&device, &path_str)
                    .await
                    .map_err(|e| e.to_string());
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
                let res = list_dir(&device, &path_str)
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device, path, res)).await;
            });
        }
        BrowserEvent::OpenDir(entry) => {
            if handles.browser.is_loading() {
                return;
            }
            if !entry.is_dir && !entry.is_symlink {
                return;
            }
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
                let res = list_dir(&device, &path_str)
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device, new_path, res)).await;
            });
        }
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
                                let name = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("file")
                                    .to_string();
                                let local = path.to_string_lossy().to_string();
                                let device_path = format!("/sdcard/Download/{name}");
                                let device = device.clone();
                                let op_tx = op_tx.clone();
                                rt.spawn(async move {
                                    let r = enqueue_push(&device, &local, &device_path)
                                        .await
                                        .map(|_| format!("Queued push of {name} to {device_path}"));
                                    let _ = op_tx.try_send((
                                        Some("ADB Push".into()),
                                        r.map_err(|e| e.to_string()),
                                    ));
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
                            let name = path
                                .file_name()
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
                                let refresh = list_dir(
                                    &device_for_fetch,
                                    &device_dir_for_async.to_string_lossy(),
                                )
                                .await
                                .map_err(|e| e.to_string());
                                let _ = dir_tx
                                    .send((device_for_fetch.clone(), device_dir_for_fetch, refresh))
                                    .await;
                            });
                        }
                    }
                }
                chooser.destroy();
            });
            chooser.show();
        }
        BrowserEvent::Download(entries) => {
            if entries.is_empty() {
                return;
            }
            let files: Vec<FsDirEntry> = entries.into_iter().filter(|e| !e.is_dir).collect();
            if files.is_empty() {
                return;
            }

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
                            Err(err) => {
                                first_err = Some(format!("copy {}: {err}", e.name));
                                break;
                            }
                        }
                    }
                    match first_err {
                        Some(err) => {
                            let _ = op_tx.try_send((None, Err(err)));
                        }
                        None => {
                            let _ = op_tx.try_send((
                                Some("Copy".into()),
                                Ok(format!("Copied {copied} file(s) to {}", dst_dir.display())),
                            ));
                        }
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
                                    let _ = enqueue_pull(&device, &src_str, &local).await.map_err(
                                        |e| tracing::warn!(error=%e, "enqueue_pull failed"),
                                    );
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
                                    let local =
                                        dest_dir.join(&e.name).to_string_lossy().to_string();
                                    let device = device.clone();
                                    rt.spawn(async move {
                                        let _ = enqueue_pull(&device, &src, &local).await.map_err(
                                            |e| tracing::warn!(error=%e, "enqueue_pull failed"),
                                        );
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
            let Some(device) = handles.selected_device.lock().clone() else {
                return;
            };
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
                    let _ = op_tx
                        .send((None, Err(format!("mkdir {target_str}: {e}"))))
                        .await;
                }
                let res = list_dir(&device_for_op, &dir_str)
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device_for_op, dir, res)).await;
            });
        }
        BrowserEvent::Rename(entry, new_name) => {
            let curr = handles.browser.current_path();
            if handles.browser.is_local_mode() {
                let dir = curr.clone();
                let mut src = curr.clone();
                src.push(&entry.name);
                let mut dst = curr.clone();
                dst.push(&new_name);
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
            let Some(device) = handles.selected_device.lock().clone() else {
                return;
            };
            let dir = handles.browser.current_path();
            let mut src = dir.clone();
            src.push(&entry.name);
            let mut dst = dir.clone();
            dst.push(&new_name);
            let src_str = src.to_string_lossy().to_string();
            let dst_str = dst.to_string_lossy().to_string();
            let op_tx = op_tx.clone();
            let device_for_op = device.clone();
            let dir_tx = dir_tx.clone();
            rt.spawn(async move {
                if let Err(e) = rename(&device_for_op, &src_str, &dst_str).await {
                    let _ = op_tx
                        .send((None, Err(format!("rename {src_str}: {e}"))))
                        .await;
                }
                let res = list_dir(&device_for_op, &dir.to_string_lossy())
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device_for_op, dir, res)).await;
            });
        }
        BrowserEvent::Delete(entries) => {
            if entries.is_empty() {
                return;
            }
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
                            let _ =
                                op_tx.try_send((None, Err(format!("delete {}: {err}", e.name))));
                        }
                    }
                    let res = list_local_dir(&dir);
                    let _ = dir_tx.try_send((LOCAL_DEVICE.to_string(), dir, res));
                });
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else {
                return;
            };
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
                let res = list_dir(&device_for_op, &dir.to_string_lossy())
                    .await
                    .map_err(|e| e.to_string());
                let _ = dir_tx.send((device_for_op, dir, res)).await;
            });
        }
        BrowserEvent::OpenExternal(path) => {
            if handles.browser.is_local_mode() {
                if let Err(e) = std::process::Command::new("xdg-open").arg(&path).spawn() {
                    let _ =
                        op_tx.try_send((None, Err(format!("xdg-open {}: {e}", path.display()))));
                }
                return;
            }
            let Some(device) = handles.selected_device.lock().clone() else {
                return;
            };
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
            let Some(device) = handles.selected_device.lock().clone() else {
                let _ = op_tx.try_send((
                    None,
                    Err("No device connected — connect a device to install the APK.".into()),
                ));
                return;
            };
            let target = curr.join(&entry.name);
            let apk_path = target.to_string_lossy().to_string();
            run_apk_install(device, apk_path, entry.name, handles, op_tx, &rt);
        }
        BrowserEvent::SideloadApkPrompt => {
            let device = match handles.selected_device.lock().clone() {
                Some(d) => d,
                None => {
                    let _ = op_tx.try_send((
                        None,
                        Err("No device connected — please connect an Android device to install APKs.".into()),
                    ));
                    return;
                }
            };
            let chooser = gtk4::FileChooserNative::builder()
                .title("Select APK to Install")
                .modal(true)
                .action(gtk4::FileChooserAction::Open)
                .build();
            chooser.set_transient_for(Some(&window));
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some("Android Packages (*.apk)"));
            filter.add_pattern("*.apk");
            filter.add_pattern("*.APK");
            chooser.add_filter(&filter);

            let handles_cb = handles.clone();
            let op_tx_cb = op_tx.clone();
            let rt_cb = rt.clone();
            chooser.connect_response(move |chooser, resp| {
                if resp == gtk4::ResponseType::Accept {
                    if let Some(file) = chooser.file() {
                        if let Some(path) = file.path() {
                            let path_str = path.to_string_lossy().to_string();
                            let name = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("app.apk")
                                .to_string();
                            run_apk_install(device.clone(), path_str, name, &handles_cb, &op_tx_cb, &rt_cb);
                        }
                    }
                }
            });
            chooser.show();
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
                format!(
                    "adb -s {} shell 'cd {} && exec $SHELL -l'",
                    device, path_str
                )
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

static MANAGER_PROXY: tokio::sync::OnceCell<adbshare_dbus_proxy::ManagerProxy<'static>> =
    tokio::sync::OnceCell::const_new();

async fn get_manager() -> anyhow::Result<&'static adbshare_dbus_proxy::ManagerProxy<'static>> {
    use zbus::{Connection, names::WellKnownName};
    MANAGER_PROXY
        .get_or_try_init(|| async {
            // ponytail: spawn fallback if D-Bus activation unavailable (no .service
            // installed, e.g. running from cargo). D-Bus activation is the primary
            // path via org.adbshare.Manager.service + systemd --user; upgrade to
            // removing this when packaged installs are the only supported path.
            ensure_daemon_running().await;
            let conn = Connection::session().await?;
            let proxy = adbshare_dbus_proxy::ManagerProxy::builder(&conn)
                .destination(WellKnownName::try_from("org.adbshare.Manager")?)?
                .build()
                .await?;
            Ok(proxy)
        })
        .await
}

/// Best-effort daemon startup for environments without D-Bus activation
/// (dev runs from `cargo`, missing package files). No-op if the name is
/// already owned — the common case once the .service + systemd unit ship.
async fn ensure_daemon_running() {
    use std::process::Stdio;
    use zbus::{Connection, names::WellKnownName};

    // Fast path: daemon already owns the bus name.
    if let Ok(conn) = Connection::session().await {
        if let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await {
            let name = WellKnownName::try_from("org.adbshare.Manager").unwrap();
            if dbus.name_has_owner(name.into()).await.unwrap_or(false) {
                return;
            }
        }
    }

    let exe = match daemon_binary_path() {
        Some(p) => p,
        None => return,
    };
    let _ = std::process::Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    // Give the daemon a moment to claim the bus name before first call.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
}

/// Locate `adb-daemon`: same dir as `adb-gui` first (dev + tarball),
/// then PATH. None if neither resolves — caller silently skips spawn.
fn daemon_binary_path() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let next_to = dir.join("adb-daemon");
            if next_to.is_file() {
                return Some(next_to);
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("adb-daemon"))
            .find(|p| p.is_file())
    })
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

async fn copy_file(device: &str, src: &str, dst: &str) -> anyhow::Result<()> {
    let proxy = get_manager().await?;
    Ok(proxy.copy_file(device, src, dst).await?)
}

#[cfg(test)]
mod copy_tests {
    use super::*;

    struct CopyManager;

    #[zbus::interface(name = "org.adbshare.Manager")]
    impl CopyManager {
        async fn copy_file(&self, device: &str, src: &str, dst: &str) -> zbus::fdo::Result<()> {
            assert_eq!((device, src, dst), ("test-phone", "/source", "/destination"));
            Err(zbus::fdo::Error::Failed(
                "completion unknown; destination may be incomplete or still copying".into(),
            ))
        }
    }

    #[test]
    fn job_info_preserves_error_and_device() {
        let dto: JobDto = serde_json::from_str(
            r#"{"id":7,"direction":"Push","source":"/src/a","destination":"/dst/a",
                "state":"Failed","bytes_done":1,"bytes_total":2,
                "error":"boom","device":"phone"}"#,
        )
        .unwrap();
        let info = JobInfo::from(dto);
        assert_eq!(info.error.as_deref(), Some("boom"));
        assert_eq!(info.device.as_deref(), Some("phone"));
        let legacy: JobDto = serde_json::from_str(
            r#"{"id":8,"direction":"Pull","source":"/a","destination":"/b",
                "state":"Completed","bytes_done":2,"bytes_total":2}"#,
        )
        .unwrap();
        let info = JobInfo::from(legacy);
        assert!(info.error.is_none());
        assert!(info.device.is_none());
    }

    #[tokio::test]
    #[ignore = "requires dbus-run-session -- cargo test -p adb-gui --locked copy_wrapper_preserves_unknown_completion -- --ignored"]
    async fn copy_wrapper_preserves_unknown_completion() {
        let server = zbus::ConnectionBuilder::session().unwrap()
            .serve_at("/org/adbshare/Manager", CopyManager).unwrap()
            .build().await.unwrap();
        let connection = zbus::Connection::session().await.unwrap();
        let proxy = ManagerProxy::builder(&connection)
            .destination(server.unique_name().unwrap().to_owned()).unwrap()
            .build().await.unwrap();
        assert!(MANAGER_PROXY.set(proxy).is_ok());
        let error = copy_file("test-phone", "/source", "/destination").await.unwrap_err();
        assert!(error.to_string().contains(
            "completion unknown; destination may be incomplete or still copying"
        ));
    }
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

async fn install_apk(device: &str, path: &str) -> anyhow::Result<String> {
    let proxy = get_manager().await?;
    Ok(proxy.install_apk(device, path).await?)
}

fn run_apk_install(
    device: String,
    apk_path: String,
    display_name: String,
    handles: &UiHandles,
    op_tx: &async_channel::Sender<(Option<String>, Result<String, String>)>,
    rt: &tokio::runtime::Handle,
) {
    let dev = device.clone();
    let path = apk_path.clone();
    let name = display_name.clone();
    let op_tx = op_tx.clone();
    let browser = handles.browser.clone();
    let rt = rt.clone();

    browser.set_status(&format!("Installing {name} on {dev}…"));

    glib::spawn_future_local(async move {
        let dev_bg = dev.clone();
        let path_bg = path.clone();
        let res = rt
            .spawn(async move { install_apk(&dev_bg, &path_bg).await })
            .await;

        browser.set_status("");

        let (title, r) = match res {
            Ok(Ok(_)) => (
                Some("APK Installed".into()),
                Ok(format!("Successfully installed '{name}' on {dev}.")),
            ),
            Ok(Err(e)) => (
                Some("Installation Failed".into()),
                Err(format!("Could not install '{name}':\n\n{e}")),
            ),
            Err(join_err) => (
                Some("Installation Failed".into()),
                Err(format!("Install task failed: {join_err}")),
            ),
        };
        let _ = op_tx.send((title, r)).await;
    });
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

    // QR-pairing stub (no new deps): echo the typed host:port back as a
    // large selectable label with a Copy button, so the user can copy it
    // to the phone. Updates live as the entry changes.
    let pairing_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let pairing_label = gtk4::Label::builder()
        .label("192.168.1.20:5555")
        .halign(gtk4::Align::Center)
        .hexpand(true)
        .selectable(true)
        .wrap(true)
        .build();
    pairing_label.add_css_class("title-2");
    pairing_label.add_css_class("monospace");
    pairing_label.add_css_class("dim-label");
    let copy_btn = gtk4::Button::with_label("Copy");
    copy_btn.set_tooltip_text(Some("Copy address to clipboard"));
    pairing_row.append(&pairing_label);
    pairing_row.append(&copy_btn);
    content.append(&pairing_row);

    let pairing_for_entry = pairing_label.clone();
    entry.connect_changed(move |e| {
        let addr = e.text().trim().to_string();
        if addr.is_empty() {
            pairing_for_entry.set_label("192.168.1.20:5555");
            pairing_for_entry.add_css_class("dim-label");
        } else {
            pairing_for_entry.set_label(&addr);
            pairing_for_entry.remove_css_class("dim-label");
        }
    });

    let entry_for_copy = entry.clone();
    copy_btn.connect_clicked(move |_| {
        let addr = entry_for_copy.text().trim().to_string();
        let text = if addr.is_empty() {
            "192.168.1.20:5555".to_string()
        } else {
            addr
        };
        if let Some(display) = gtk4::gdk::Display::default() {
            display.clipboard().set_text(&text);
        }
    });

    let camera_hint = gtk4::Label::builder()
        .label("Open your phone camera / Wi-Fi pairing screen and type this address, or tap Copy.")
        .xalign(0.0)
        .wrap(true)
        .build();
    camera_hint.add_css_class("dim-label");
    content.append(&camera_hint);

    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
    let connect_btn = dialog.add_button("Connect", gtk4::ResponseType::Ok);
    connect_btn.add_css_class("suggested-action");

    let op_tx = op_tx.clone();
    dialog.connect_response(move |d, resp| {
        if resp == gtk4::ResponseType::Ok {
            let addr = entry.text().trim().to_string();
            if addr.is_empty() {
                return;
            }
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

static TRANSFER_POLICY: std::sync::OnceLock<parking_lot::Mutex<String>> = std::sync::OnceLock::new();
static TRANSFER_VERIFY: std::sync::OnceLock<parking_lot::Mutex<bool>> = std::sync::OnceLock::new();

fn transfer_policy() -> (String, bool) {
    let policy = TRANSFER_POLICY.get_or_init(|| parking_lot::Mutex::new("skip".to_string()));
    let verify = TRANSFER_VERIFY.get_or_init(|| parking_lot::Mutex::new(false));
    (policy.lock().clone(), *verify.lock())
}

fn set_transfer_policy(policy: &str, verify: bool) {
    *TRANSFER_POLICY.get_or_init(|| parking_lot::Mutex::new("skip".to_string())).lock() =
        policy.to_string();
    *TRANSFER_VERIFY.get_or_init(|| parking_lot::Mutex::new(false)).lock() = verify;
}

async fn enqueue_push(device: &str, local: &str, device_path: &str) -> anyhow::Result<u64> {
    let proxy = get_manager().await?;
    let (policy, verify) = transfer_policy();
    match proxy.enqueue_push_with_options(device, local, device_path, &policy, verify).await {
        Ok(id) => Ok(id),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
        {
            Ok(proxy.enqueue_push(device, local, device_path).await?)
        }
        Err(e) => Err(e.into()),
    }
}

async fn enqueue_pull(device: &str, device_path: &str, local: &str) -> anyhow::Result<u64> {
    let proxy = get_manager().await?;
    let (policy, verify) = transfer_policy();
    match proxy.enqueue_pull_with_options(device, device_path, local, &policy, verify).await {
        Ok(id) => Ok(id),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
        {
            Ok(proxy.enqueue_pull(device, device_path, local).await?)
        }
        Err(e) => Err(e.into()),
    }
}

async fn retry_failed() -> anyhow::Result<u64> {
    let proxy = get_manager().await?;
    Ok(proxy.retry_failed().await?)
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
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    device: Option<String>,
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
            error: j.error,
            device: j.device,
        }
    }
}
