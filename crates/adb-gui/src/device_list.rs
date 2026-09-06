//! Sidebar: source list with three unambiguous groups.
//!
//! User flow, top to bottom:
//!   1. "Phones & tablets" — pick *which* device you are talking to.
//!      Empty state teaches the 3 setup steps instead of a dead label.
//!   2. "Phone folders" — pick *where on that phone* to browse.
//!      Disabled until a phone is selected, so it can never mislead.
//!   3. "This computer" — local Linux places. Visually separated so it
//!      is never confused with phone storage.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;

#[derive(Debug, Clone)]
pub enum SidebarEvent {
    SelectDevice(String),
    SelectPlace(PathBuf),
    /// Browse the local Linux filesystem (sidebar "This computer").
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

    /// Short transport label shown next to the name.
    pub fn transport_label(&self) -> &'static str {
        if self.transport == "wifi" { "Wi-Fi" } else { "USB" }
    }
}

/// "23.4 / 128 GB" style capacity string.
fn format_capacity((used, total): (u64, u64)) -> String {
    let mut unit = "GB";
    let mut div = 1024.0_f64.powi(3);
    if total as f64 >= 1024.0_f64.powi(4) {
        unit = "TB";
        div = 1024.0_f64.powi(4);
    }
    format!("{:.1} of {:.0} {} used", used as f64 / div, total as f64 / div, unit)
}

fn heading(text: &str) -> gtk4::Label {
    let lbl = gtk4::Label::builder().label(text).xalign(0.0).build();
    lbl.add_css_class("sidebar-heading");
    lbl
}

/// One connected phone: status dot + name + transport badge on row 1,
/// mono serial + battery on row 2, storage bar + caption on row 3.
fn device_row(entry: &DeviceEntry, selected: bool) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::builder()
        .activatable(true)
        .name(&entry.serial)
        .build();
    row.add_css_class("device-card");
    if selected {
        row.add_css_class("active");
    }

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 4);

    // Row 1: status dot + name .... transport badge.
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    top.set_valign(gtk4::Align::Center);
    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.add_css_class("led");
    dot.add_css_class(if entry.transport == "wifi" {
        "led-wifi"
    } else {
        "led-on"
    });
    dot.set_valign(gtk4::Align::Center);
    dot.set_size_request(8, 8);
    top.append(&dot);

    let icon = gtk4::Image::from_icon_name("phone-symbolic");
    icon.set_pixel_size(16);
    icon.add_css_class(if selected {
        "selected-icon"
    } else {
        "sidebar-icon-dim"
    });
    top.append(&icon);

    let name_lbl = gtk4::Label::builder()
        .label(entry.display_name())
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    name_lbl.add_css_class("device-name");
    top.append(&name_lbl);

    let chip = gtk4::Label::new(Some(entry.transport_label()));
    chip.add_css_class(if selected {
        "usb-badge"
    } else {
        "usb-badge-muted"
    });
    chip.set_valign(gtk4::Align::Center);
    top.append(&chip);
    vbox.append(&top);

    // Row 2: serial + battery, both dimmed mono.
    let mid = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    mid.set_margin_start(24);
    let serial_lbl = gtk4::Label::builder()
        .label(&entry.serial)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    serial_lbl.add_css_class("device-subline");
    mid.append(&serial_lbl);
    if let Some(pct) = entry.battery_pct {
        let batt = gtk4::Label::new(Some(&format!("{}%", pct)));
        batt.add_css_class("battery-pill");
        if pct <= 20 {
            batt.add_css_class("low");
        }
        batt.set_valign(gtk4::Align::Center);
        mid.append(&batt);
    }
    vbox.append(&mid);

    // Row 3: storage bar + caption, only when the daemon knows it.
    if let Some((used, total)) = entry.storage.filter(|(_, t)| *t > 0) {
        let frac = ((used as f64) / (total as f64)).clamp(0.0, 1.0);
        let bar = gtk4::ProgressBar::new();
        bar.set_show_text(false);
        bar.set_fraction(frac);
        bar.add_css_class("device-storage-bar");
        bar.set_margin_start(24);
        bar.set_margin_top(2);
        vbox.append(&bar);
        let cap = gtk4::Label::builder()
            .label(format_capacity((used, total)))
            .xalign(0.0)
            .build();
        cap.add_css_class("device-subline");
        cap.set_margin_start(24);
        vbox.append(&cap);
    }

    row.set_child(Some(&vbox));
    row
}

/// Simple icon + label row used for folder shortcuts.
fn shortcut_row(key: &str, label: &str, icon_name: &str, dim_icon: bool) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::builder()
        .activatable(true)
        .name(key)
        .build();
    row.add_css_class("sidebar-row");
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    box_.set_margin_start(2);
    box_.set_margin_end(2);
    let img = gtk4::Image::from_icon_name(icon_name);
    img.set_pixel_size(16);
    img.add_css_class(if dim_icon {
        "sidebar-icon-dim"
    } else {
        "sidebar-icon-places"
    });
    box_.append(&img);
    let lbl = gtk4::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    box_.append(&lbl);
    row.set_child(Some(&box_));
    row
}

pub struct DeviceList {
    root: gtk4::Box,
    device_list_box: gtk4::ListBox,
    phone_folders_box: gtk4::ListBox,
    phone_hint: gtk4::Label,
    on_event: Rc<RefCell<Option<Box<dyn Fn(SidebarEvent)>>>>,
}

impl DeviceList {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        root.set_margin_start(8);
        root.set_margin_end(8);
        root.set_margin_top(8);
        root.set_margin_bottom(8);

        // ---- Group 1: which phone? ----
        root.append(&heading("Phones & tablets"));
        let device_list_box = gtk4::ListBox::new();
        device_list_box.set_selection_mode(gtk4::SelectionMode::Single);
        device_list_box.add_css_class("sidebar-list");
        root.append(&device_list_box);

        let on_event: Rc<RefCell<Option<Box<dyn Fn(SidebarEvent)>>>> =
            Rc::new(RefCell::new(None));

        // Always-visible Wi-Fi connect action directly under the phones.
        // (Explicit icon+label child: GTK4 renders either/or, never both.)
        {
            let connect_btn = gtk4::Button::new();
            let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            hbox.set_halign(gtk4::Align::Center);
            hbox.append(&gtk4::Image::from_icon_name("network-wireless-symbolic"));
            hbox.append(&gtk4::Label::new(Some("Connect via Wi-Fi…")));
            connect_btn.set_child(Some(&hbox));
            connect_btn.add_css_class("connect-button");
            connect_btn.set_margin_top(4);
            connect_btn.set_tooltip_text(Some(
                "Pair once on the phone, then connect by IP address",
            ));
            let on_ev = on_event.clone();
            connect_btn.connect_clicked(move |_| {
                if let Some(cb) = on_ev.borrow().as_ref() {
                    cb(SidebarEvent::ConnectIp);
                }
            });
            root.append(&connect_btn);
        }

        // ---- Group 2: where on the phone? ----
        root.append(&heading("Phone folders"));
        let hint = gtk4::Label::builder()
            .label("Select a phone above to browse its folders.")
            .xalign(0.0)
            .wrap(true)
            .build();
        hint.add_css_class("sidebar-hint");
        root.append(&hint);

        let phone_folders_box = gtk4::ListBox::new();
        phone_folders_box.set_selection_mode(gtk4::SelectionMode::None);
        phone_folders_box.add_css_class("sidebar-list");
        // (label, icon, device path)
        let phone_items: &[(&str, &str, &str)] = &[
            ("Internal storage", "phone-symbolic", "/sdcard"),
            ("Download", "folder-download-symbolic", "/sdcard/Download"),
            ("Camera", "camera-photo-symbolic", "/sdcard/DCIM"),
            ("Pictures", "folder-pictures-symbolic", "/sdcard/Pictures"),
            ("Music", "folder-music-symbolic", "/sdcard/Music"),
            ("Documents", "folder-documents-symbolic", "/sdcard/Documents"),
        ];
        for (label, icon, path) in phone_items {
            phone_folders_box.append(&shortcut_row(
                &format!("dev:{path}"),
                label,
                icon,
                false,
            ));
        }
        root.append(&phone_folders_box);

        // ---- Group 3: this computer (unmistakably local) ----
        root.append(&heading("This computer"));
        let local_box = gtk4::ListBox::new();
        local_box.set_selection_mode(gtk4::SelectionMode::None);
        local_box.add_css_class("sidebar-list");
        for (label, icon, key) in [
            ("Home", "user-home-symbolic", "local:HOME"),
            ("Downloads", "folder-download-symbolic", "local:DOWNLOADS"),
            ("Trash", "user-trash-symbolic", "local:TRASH"),
        ] {
            local_box.append(&shortcut_row(key, label, icon, true));
        }
        root.append(&local_box);

        // --- wiring ---
        let on_ev_dev = on_event.clone();
        device_list_box.connect_row_activated(move |_lb, row| {
            let name = row.widget_name().to_string();
            if !name.is_empty() && name != "__empty__" {
                if let Some(cb) = on_ev_dev.borrow().as_ref() {
                    cb(SidebarEvent::SelectDevice(name));
                }
            }
        });

        let on_ev_phone = on_event.clone();
        phone_folders_box.connect_row_activated(move |_lb, row| {
            let key = row.widget_name().to_string();
            if let Some(path) = key.strip_prefix("dev:") {
                if let Some(cb) = on_ev_phone.borrow().as_ref() {
                    cb(SidebarEvent::SelectPlace(PathBuf::from(path)));
                }
            }
        });

        let on_ev_local = on_event.clone();
        local_box.connect_row_activated(move |_lb, row| {
            let key = row.widget_name().to_string();
            let target = match key.strip_prefix("local:") {
                Some("HOME") => dirs::home_dir(),
                Some("DOWNLOADS") => dirs::download_dir().or_else(dirs::home_dir),
                Some("TRASH") => dirs::home_dir().map(|h| h.join(".local/share/Trash/files")),
                Some(p) => Some(PathBuf::from(p)),
                None => None,
            };
            if let Some(path) = target {
                if let Some(cb) = on_ev_local.borrow().as_ref() {
                    cb(SidebarEvent::SelectLocal(path));
                }
            }
        });

        Self {
            root,
            device_list_box,
            phone_folders_box,
            phone_hint: hint,
            on_event,
        }
    }

    pub fn on_event<F: Fn(SidebarEvent) + 'static>(&self, f: F) {
        *self.on_event.borrow_mut() = Some(Box::new(f));
    }

    pub fn attach(&self, parent: &gtk4::Box) {
        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();
        scrolled.set_child(Some(&self.root));
        scrolled.set_propagate_natural_width(true);
        parent.append(&scrolled);
    }

    /// Honest first-run state: what to do, in order. No fake devices.
    pub fn populate_defaults(&self) {
        self.set_devices(&[], None);
    }

    /// Replace the phone list. Empty => onboarding steps.
    pub fn set_devices(&self, devices: &[DeviceEntry], selected: Option<&str>) {
        while let Some(child) = self.device_list_box.first_child() {
            self.device_list_box.remove(&child);
        }
        if devices.is_empty() {
            let row = gtk4::ListBoxRow::builder()
                .sensitive(false)
                .name("__empty__")
                .build();
            row.add_css_class("onboarding-card");
            let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            vbox.set_margin_start(4);
            vbox.set_margin_end(4);
            vbox.set_margin_top(4);
            vbox.set_margin_bottom(4);
            let title = gtk4::Label::builder()
                .label("No phone connected")
                .xalign(0.0)
                .build();
            title.add_css_class("onboarding-title");
            vbox.append(&title);
            for (n, step) in [
                "Connect the phone with a USB cable.",
                "On the phone: allow USB debugging, then tap “Allow”.",
                "Or use “Connect via Wi-Fi” below.",
            ]
            .iter()
            .enumerate()
            {
                let step_lbl = gtk4::Label::builder()
                    .label(format!("{}. {}", n + 1, step))
                    .xalign(0.0)
                    .wrap(true)
                    .build();
                step_lbl.add_css_class("onboarding-step");
                vbox.append(&step_lbl);
            }
            row.set_child(Some(&vbox));
            self.device_list_box.append(&row);
        } else {
            let mut selected_row: Option<gtk4::ListBoxRow> = None;
            let mut first_row: Option<gtk4::ListBoxRow> = None;
            for d in devices {
                let is_sel = selected.is_some_and(|s| s == d.serial);
                let row = device_row(d, is_sel);
                if first_row.is_none() {
                    first_row = Some(row.clone());
                }
                if is_sel {
                    selected_row = Some(row.clone());
                }
                self.device_list_box.append(&row);
            }
            if let Some(row) = selected_row.or(first_row) {
                self.device_list_box.select_row(Some(&row));
            }
        }
        self.set_phone_folders_enabled(selected.is_some() || !devices.is_empty());
    }

    /// Phone folders only make sense with a phone; dim them otherwise so
    /// clicking them can never show a confusing error. The hint explains
    /// what to do only while there is nothing to browse.
    pub fn set_phone_folders_enabled(&self, enabled: bool) {
        self.phone_folders_box.set_sensitive(enabled);
        self.phone_folders_box.set_opacity(if enabled { 1.0 } else { 0.45 });
        self.phone_hint.set_visible(!enabled);
    }

    pub fn clear(&self) {
        while let Some(child) = self.device_list_box.first_child() {
            self.device_list_box.remove(&child);
        }
    }
}

impl Default for DeviceList {
    fn default() -> Self {
        Self::new()
    }
}
