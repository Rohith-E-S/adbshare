//! Sidebar list of devices and Nautilus-style quick-access places,
//! matching the stitch "Libadwaita Nautilus Dark" mockup row-for-row.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;

#[derive(Debug, Clone)]
pub enum SidebarEvent {
    SelectDevice(String),
    SelectPlace(PathBuf),
    /// Browse the local Linux filesystem (sidebar "Linux Root").
    SelectLocal(PathBuf),
    ConnectIp,
}

/// A device as reported by the daemon (`list_devices` + `device_info`).
#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub serial: String,
    pub model: Option<String>,
    /// "usb" or "wifi" (inferred from the serial by the daemon).
    pub transport: &'static str,
    /// (used, total) bytes on /sdcard, if the device reported them.
    pub storage: Option<(u64, u64)>,
    /// Battery percentage, if reported.
    pub battery_pct: Option<u8>,
}

impl DeviceEntry {
    pub fn display_name(&self) -> &str {
        self.model.as_deref().unwrap_or(&self.serial)
    }
}

/// "23.4 / 128 GB" style capacity string.
fn format_capacity((used, total): (u64, u64)) -> String {
    let mut u = used as f64;
    let mut t = total as f64;
    let mut unit = "GB";
    let mut div = 1024.0 * 1024.0 * 1024.0;
    if t >= 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        unit = "TB";
        div = 1024.0 * 1024.0 * 1024.0 * 1024.0;
    }
    u /= div;
    t /= div;
    format!("{:.1} / {:.0} {}", u, t, unit)
}

/// Hairline separator between sidebar groups.
fn separator_row() -> gtk4::Separator {
    let sep = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    sep.add_css_class("sidebar-separator");
    sep
}

pub struct DeviceList {
    root: gtk4::Box,
    device_list_box: gtk4::ListBox,
    on_event: Rc<RefCell<Option<Box<dyn Fn(SidebarEvent)>>>>,
}

/// A connected-device card (Stitch "Signal Deck"): elevated card with
/// a status LED, name + transport/battery chips, mono serial line, and
/// a thin glowing storage capacity bar with a capacity caption.
fn device_row(
    serial: &str,
    display_name: &str,
    icon_name: &str,
    chip: Option<&str>,
    status_left: &str,
    status_right: Option<&str>,
    selected: bool,
    storage: Option<(u64, u64)>,
    battery_pct: Option<u8>,
) -> gtk4::ListBoxRow {
    // The row identity is the serial: SelectDevice carries it to the daemon.
    let row = gtk4::ListBoxRow::builder()
        .activatable(true)
        .name(serial)
        .build();
    row.add_css_class("device-card");
    if !selected {
        row.add_css_class("dimmed");
    }

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 5);

    // Row 1: LED + device icon + name ... transport badge + battery pill.
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 7);
    let led = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    led.add_css_class("led");
    led.add_css_class(if chip == Some("Wi-Fi") { "led-wifi" } else { "led-on" });
    led.set_valign(gtk4::Align::Center);
    led.set_size_request(8, 8);
    top.append(&led);

    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.set_pixel_size(15);
    icon.add_css_class(if selected { "selected-icon" } else { "sidebar-icon-dim" });
    top.append(&icon);

    let name_lbl = gtk4::Label::builder()
        .label(display_name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    name_lbl.add_css_class("device-name");
    top.append(&name_lbl);

    if let Some(pct) = battery_pct {
        let batt_lbl = gtk4::Label::new(Some(&format!("{}%", pct)));
        if pct <= 20 {
            batt_lbl.add_css_class("battery-pill");
            batt_lbl.add_css_class("low");
        } else {
            batt_lbl.add_css_class("battery-pill");
        }
        batt_lbl.set_valign(gtk4::Align::Center);
        top.append(&batt_lbl);
    }
    if let Some(chip_text) = chip {
        let chip_lbl = gtk4::Label::new(Some(chip_text));
        chip_lbl.add_css_class(if selected { "usb-badge" } else { "usb-badge-muted" });
        chip_lbl.set_valign(gtk4::Align::Center);
        top.append(&chip_lbl);
    }
    vbox.append(&top);

    // Row 2: mono status line (serial / auth state) + capacity text.
    let mid = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    mid.set_margin_start(15);
    let left = gtk4::Label::builder()
        .label(status_left)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    left.add_css_class("device-subline");
    mid.append(&left);
    if let Some(right) = status_right {
        let right_lbl = gtk4::Label::builder()
            .label(right)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        right_lbl.add_css_class("device-subline");
        mid.append(&right_lbl);
    }
    vbox.append(&mid);

    // Row 3: thin glowing capacity bar (accent green fill).
    if let Some((used, total)) = storage.filter(|(_, t)| *t > 0) {
        let bar = gtk4::ProgressBar::new();
        bar.set_show_text(false);
        bar.set_fraction(((used as f64) / (total as f64)).clamp(0.0, 1.0));
        bar.add_css_class("device-storage-bar");
        bar.set_margin_top(2);
        vbox.append(&bar);
    }

    row.set_child(Some(&vbox));
    row
}

fn connect_ip_row() -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::builder()
        .activatable(true)
        .name("__connect_ip__")
        .build();
    row.add_css_class("sidebar-row");

    // Ghost action button look (Stitch: "+ Connect device" dashed tile).
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    box_.set_halign(gtk4::Align::Center);
    let icon = gtk4::Image::from_icon_name("list-add-symbolic");
    icon.set_pixel_size(14);
    icon.add_css_class("sidebar-icon-dim");
    box_.append(&icon);
    let lbl = gtk4::Label::builder()
        .label("Connect device")
        .xalign(0.0)
        .build();
    box_.append(&lbl);
    row.add_css_class("connect-ghost");
    row.set_child(Some(&box_));
    row
}

/// Storage-location row: icon + label on the left, mono path on the right,
/// optional eject button (mockup "SD Card (External)").
fn storage_row(label: &str, path_display: &str, icon_name: &str, ejectable: bool) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::builder()
        .activatable(true)
        .name(label)
        .build();
    row.add_css_class("sidebar-row");

    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.set_pixel_size(16);
    icon.add_css_class("sidebar-icon-places");
    box_.append(&icon);

    let lbl = gtk4::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    box_.append(&lbl);

    if ejectable {
        let eject = gtk4::Button::from_icon_name("media-eject-symbolic");
        eject.add_css_class("flat");
        eject.set_tooltip_text(Some("Eject"));
        eject.set_valign(gtk4::Align::Center);
        box_.append(&eject);
    } else {
        let path_lbl = gtk4::Label::builder()
            .label(path_display)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        path_lbl.add_css_class("storage-path");
        path_lbl.set_valign(gtk4::Align::Center);
        box_.append(&path_lbl);
    }

    row.set_child(Some(&box_));
    row
}

impl DeviceList {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        // Extra start margin: keeps headings/rows clear of the left edge even
        // when the compositor crops a few px off a narrow tiled window.
        root.set_margin_start(4);
        root.set_margin_end(8);
        root.set_margin_top(12);
        root.set_margin_bottom(8);

        // Group 1: Devices & ADB — first in the rail, per the Stitch
        // "Centered Dock" variant (rich device cards + connect action).
        let devices_heading = gtk4::Label::builder()
            .label("Devices")
            .xalign(0.0)
            .build();
        devices_heading.add_css_class("sidebar-heading");
        root.append(&devices_heading);
        let device_list_box = gtk4::ListBox::new();
        device_list_box.set_selection_mode(gtk4::SelectionMode::Single);
        device_list_box.add_css_class("sidebar-list");
        root.append(&device_list_box);

        root.append(&separator_row());

        // Group 2: Quick Access (Stitch rail) — Android folders that browse
        // the selected device, plus the two local system places. Row names
        // encode the target: "dev:<path>" or "local:<path>".
        let quick_heading = gtk4::Label::builder()
            .label("Quick Access")
            .xalign(0.0)
            .build();
        quick_heading.add_css_class("sidebar-heading");
        root.append(&quick_heading);
        let quick_list_box = gtk4::ListBox::new();
        quick_list_box.set_selection_mode(gtk4::SelectionMode::None);
        quick_list_box.add_css_class("sidebar-list");

        #[derive(Clone)]
        enum QuickTarget {
            Device(&'static str),
            LocalLocal(&'static str),
            Home,
            Trash,
        }
        impl QuickTarget {
            fn key(&self) -> String {
                match self {
                    QuickTarget::Device(p) => format!("dev:{}", p),
                    QuickTarget::LocalLocal(p) => format!("local:{}", p),
                    QuickTarget::Home => "local:HOME".to_string(),
                    QuickTarget::Trash => "local:TRASH".to_string(),
                }
            }
        }

        let quick_items: &[(&str, &str, QuickTarget)] = &[
            ("Downloads", "folder-download-symbolic", QuickTarget::Device("/sdcard/Download")),
            ("Camera", "camera-photo-symbolic", QuickTarget::Device("/sdcard/DCIM")),
            ("Documents", "folder-documents-symbolic", QuickTarget::Device("/sdcard/Documents")),
            ("Pictures", "folder-pictures-symbolic", QuickTarget::Device("/sdcard/Pictures")),
            ("Music", "folder-music-symbolic", QuickTarget::Device("/sdcard/Music")),
            ("Home", "user-home-symbolic", QuickTarget::Home),
            ("Trash", "user-trash-symbolic", QuickTarget::Trash),
        ];

        for (label, icon, target) in quick_items {
            let row = gtk4::ListBoxRow::builder()
                .activatable(true)
                .name(target.key())
                .build();
            row.add_css_class("sidebar-row");
            let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let img = gtk4::Image::from_icon_name(icon);
            img.set_pixel_size(16);
            if *label == "Trash" {
                img.add_css_class("sidebar-icon-trash");
            } else {
                img.add_css_class("sidebar-icon-places");
            }
            box_.append(&img);
            let lbl = gtk4::Label::builder().label(*label).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).build();
            box_.append(&lbl);
            row.set_child(Some(&box_));
            quick_list_box.append(&row);
        }
        root.append(&quick_list_box);

        root.append(&separator_row());

        // Section 3: Storage Locations
        let storage_heading = gtk4::Label::builder()
            .label("On this device")
            .xalign(0.0)
            .build();
        storage_heading.add_css_class("sidebar-heading");
        root.append(&storage_heading);
        let storage_list_box = gtk4::ListBox::new();
        storage_list_box.set_selection_mode(gtk4::SelectionMode::Single);
        storage_list_box.add_css_class("sidebar-list");

        let storage_items: &[(&str, &str, &str, bool)] = &[
            ("Internal Storage", "/sdcard", "phone-symbolic", false),
            ("App Data", "/Android/data", "view-app-grid-symbolic", false),
            ("SD Card (External)", "/storage", "sd-card-symbolic", true),
            ("Linux Root", "/", "drive-harddisk-symbolic", false),
        ];

        for (label, path_display, icon, ejectable) in storage_items {
            storage_list_box.append(&storage_row(label, path_display, icon, *ejectable));
        }
        root.append(&storage_list_box);

        let on_event: Rc<RefCell<Option<Box<dyn Fn(SidebarEvent)>>>> = Rc::new(RefCell::new(None));

        // Connect Quick Access rows: "dev:<path>" browses the selected
        // device, "local:..." browses the Linux filesystem.
        let on_ev_place = on_event.clone();
        quick_list_box.connect_row_activated(move |_lb, row| {
            let key = row.widget_name().to_string();
            if let Some(path) = key.strip_prefix("dev:") {
                if let Some(cb) = on_ev_place.borrow().as_ref() {
                    cb(SidebarEvent::SelectPlace(PathBuf::from(path)));
                }
                return;
            }
            let target = if let Some(path) = key.strip_prefix("local:") {
                match path {
                    "HOME" => match dirs::home_dir() {
                        Some(h) => h,
                        None => return,
                    },
                    "TRASH" => match dirs::home_dir() {
                        Some(h) => h.join(".local/share/Trash/files"),
                        None => return,
                    },
                    p => PathBuf::from(p),
                }
            } else {
                return;
            };
            if let Some(cb) = on_ev_place.borrow().as_ref() {
                cb(SidebarEvent::SelectLocal(target));
            }
        });

        // Connect storage locations row activated
        let on_ev_storage = on_event.clone();
        storage_list_box.connect_row_activated(move |_lb, row| {
            let label = row.widget_name().to_string();
            match label.as_str() {
                "Linux Root" => {
                    if let Some(cb) = on_ev_storage.borrow().as_ref() {
                        cb(SidebarEvent::SelectLocal(PathBuf::from("/")));
                    }
                }
                "Internal Storage" => {
                    if let Some(cb) = on_ev_storage.borrow().as_ref() {
                        cb(SidebarEvent::SelectPlace(PathBuf::from("/sdcard")));
                    }
                }
                "App Data" => {
                    if let Some(cb) = on_ev_storage.borrow().as_ref() {
                        cb(SidebarEvent::SelectPlace(PathBuf::from("/sdcard/Android/data")));
                    }
                }
                "SD Card (External)" => {
                    if let Some(cb) = on_ev_storage.borrow().as_ref() {
                        cb(SidebarEvent::SelectPlace(PathBuf::from("/storage")));
                    }
                }
                _ => {}
            }
        });

        // Connect device row activated
        let on_ev_dev = on_event.clone();
        device_list_box.connect_row_activated(move |_lb, row| {
            let name = row.widget_name().to_string();
            if name == "__connect_ip__" {
                if let Some(cb) = on_ev_dev.borrow().as_ref() {
                    cb(SidebarEvent::ConnectIp);
                }
            } else if !name.is_empty() {
                if let Some(cb) = on_ev_dev.borrow().as_ref() {
                    cb(SidebarEvent::SelectDevice(name));
                }
            }
        });

        Self {
            root,
            device_list_box,
            on_event,
        }
    }

    pub fn on_event<F: Fn(SidebarEvent) + 'static>(&self, f: F) {
        *self.on_event.borrow_mut() = Some(Box::new(f));
    }

    pub fn device_list_box(&self) -> &gtk4::ListBox { &self.device_list_box }

    pub fn attach(&self, parent: &gtk4::Box) {
        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();
        scrolled.set_child(Some(&self.root));
        // The sidebar must never sit horizontally scrolled: if the content
        // transiently measured wider than the viewport (e.g. during a resize
        // from a narrow tile), a stale hadjustment value shifts the whole
        // sidebar left and clips the first characters at the window edge.
        let adj = scrolled.hadjustment();
        adj.connect_changed(|a| a.set_value(0.0));
        parent.append(&scrolled);
    }

    /// Show the honest empty state: no devices connected yet.
    pub fn populate_defaults(&self) {
        self.clear();
        let row = gtk4::ListBoxRow::builder().sensitive(false).build();
        row.add_css_class("sidebar-row");
        let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let lbl = gtk4::Label::builder()
            .label("No devices — plug in via USB")
            .xalign(0.0)
            .build();
        lbl.add_css_class("device-subline");
        box_.append(&lbl);
        row.set_child(Some(&box_));
        self.device_list_box.append(&row);
        self.device_list_box.append(&connect_ip_row());
    }

    /// Replace the device list with real devices reported by the daemon.
    /// Re-selects `selected` if it is still present.
    pub fn set_devices(&self, devices: &[DeviceEntry], selected: Option<&str>) {
        self.clear();
        let mut first_row: Option<gtk4::ListBoxRow> = None;
        let mut selected_row: Option<gtk4::ListBoxRow> = None;
        for d in devices {
            let is_sel = selected.is_some_and(|s| s == d.serial);
            let storage_right = match (d.transport, d.storage) {
                ("usb", Some(cap)) => Some(format_capacity(cap)),
                _ => None,
            };
            let row = device_row(
                &d.serial,
                d.display_name(),
                "phone-symbolic",
                Some(if d.transport == "wifi" { "Wi-Fi" } else { "USB" }),
                if d.transport == "wifi" { &d.serial } else { "ADB Authorized" },
                storage_right.as_deref(),
                is_sel,
                d.storage,
                d.battery_pct,
            );
            if first_row.is_none() {
                first_row = Some(row.clone());
            }
            if is_sel {
                selected_row = Some(row.clone());
            }
            self.device_list_box.append(&row);
        }
        self.device_list_box.append(&connect_ip_row());
        if let Some(row) = selected_row.or(first_row) {
            self.device_list_box.select_row(Some(&row));
        }
    }

    pub fn clear(&self) {
        while let Some(child) = self.device_list_box.first_child() {
            self.device_list_box.remove(&child);
        }
    }
}

impl Default for DeviceList {
    fn default() -> Self { Self::new() }
}
