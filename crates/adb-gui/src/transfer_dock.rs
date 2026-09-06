//! Bottom transfer bar: a slim full-width strip pinned to the bottom of
//! the window while copies run. Aggregate speed + up to MAX_ROWS mini rows
//! + pause/cancel. Clicking it opens the full Transfers list. Hidden
//! entirely when nothing is running, so it never covers files.

use gtk4::prelude::*;

use crate::transfer_view::JobInfo;

/// How many mini rows the dock shows at once (overflow opens the
/// full Operations & Transfers popover).
const MAX_ROWS: usize = 3;

pub struct TransferDock {
    pub root: gtk4::Box,
    pub pause_button: gtk4::Button,
    pub cancel_button: gtk4::Button,
    summary_label: gtk4::Label,
    speed_label: gtk4::Label,
    rows_box: gtk4::Box,
}

impl TransferDock {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        root.add_css_class("transfer-dock");
        root.set_halign(gtk4::Align::Fill);
        root.set_valign(gtk4::Align::End);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_margin_bottom(8);
        root.set_visible(false);

        // Left: icon + "2 files copying" above the aggregate speed readout.
        let left_icon = gtk4::Image::from_icon_name("emblem-synchronizing-symbolic");
        left_icon.set_pixel_size(20);
        left_icon.set_valign(gtk4::Align::Center);
        root.append(&left_icon);
        // Left: "2 files copying" above the aggregate speed readout.
        let left = gtk4::Box::new(gtk4::Orientation::Vertical, 1);
        left.set_valign(gtk4::Align::Center);
        let summary_label = gtk4::Label::new(Some("Transferring"));
        summary_label.add_css_class("dock-summary");
        summary_label.set_xalign(0.0);
        left.append(&summary_label);
        let speed_label = gtk4::Label::new(Some("0 B/s"));
        speed_label.add_css_class("dock-speed");
        speed_label.set_xalign(0.0);
        left.append(&speed_label);
        root.append(&left);

        let sep1 = gtk4::Separator::new(gtk4::Orientation::Vertical);
        sep1.add_css_class("dock-separator");
        sep1.set_valign(gtk4::Align::Center);
        root.append(&sep1);

        // Middle: up to MAX_ROWS mini rows (name, percent, bar).
        let rows_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        rows_box.set_valign(gtk4::Align::Center);
        root.append(&rows_box);

        let sep2 = gtk4::Separator::new(gtk4::Orientation::Vertical);
        sep2.add_css_class("dock-separator");
        sep2.set_valign(gtk4::Align::Center);
        root.append(&sep2);

        // Right: pause / cancel icon buttons.
        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        actions.set_valign(gtk4::Align::Center);
        let pause_button = gtk4::Button::from_icon_name("media-playback-pause-symbolic");
        pause_button.set_tooltip_text(Some("Pause all transfers"));
        actions.append(&pause_button);
        let cancel_button = gtk4::Button::from_icon_name("process-stop-symbolic");
        cancel_button.add_css_class("destructive");
        cancel_button.set_tooltip_text(Some("Cancel all transfers"));
        actions.append(&cancel_button);
        root.append(&actions);

        Self { root, pause_button, cancel_button, summary_label, speed_label, rows_box }
    }

    /// Rebuild the dock from the current job list. Only Running/Pending
    /// jobs count; with none active the dock hides itself.
    pub fn update(&self, jobs: &[JobInfo]) {
        let active: Vec<&JobInfo> = jobs
            .iter()
            .filter(|j| j.state == "Running" || j.state == "Pending")
            .collect();

        if active.is_empty() {
            self.root.set_visible(false);
            return;
        }
        self.root.set_visible(true);

        let n = active.len();
        self.summary_label.set_label(&format!(
            "{} file{} copying",
            n,
            if n == 1 { "" } else { "s" }
        ));

        let total_speed: u64 = active.iter().map(|j| j.speed_bps).sum();
        self.speed_label.set_label(&human_speed(total_speed));

        while let Some(child) = self.rows_box.first_child() {
            self.rows_box.remove(&child);
        }
        for j in active.iter().take(MAX_ROWS) {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);

            let arrow = match j.direction.as_str() {
                "Push" => "↑ ",
                "Pull" => "↓ ",
                _ => "",
            };
            let name = gtk4::Label::builder()
                .label(format!("{}{}", arrow, j.name))
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .width_chars(24)
                .max_width_chars(24)
                .build();
            name.add_css_class("dock-mini-name");
            row.append(&name);

            let pct = if j.bytes_total > 0 { j.bytes_done * 100 / j.bytes_total } else { 0 };
            let pct_lbl = gtk4::Label::new(Some(&format!("{}%", pct)));
            pct_lbl.add_css_class("dock-mini-pct");
            pct_lbl.set_valign(gtk4::Align::Center);
            row.append(&pct_lbl);

            let bar = gtk4::ProgressBar::new();
            bar.set_show_text(false);
            let frac = if j.bytes_total > 0 {
                (j.bytes_done as f64 / j.bytes_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            bar.set_fraction(frac);
            bar.set_width_request(110);
            bar.set_valign(gtk4::Align::Center);
            row.append(&bar);

            self.rows_box.append(&row);
        }
        if n > MAX_ROWS {
            let more = gtk4::Label::new(Some(&format!("+{} more", n - MAX_ROWS)));
            more.add_css_class("dock-mini-pct");
            more.set_xalign(0.0);
            self.rows_box.append(&more);
        }
    }
}

/// "84.2 MB/s" style aggregate readout (matches the Stitch mockup).
fn human_speed(bps: u64) -> String {
    let s = bps as f64 / 1_048_576.0;
    if s >= 1.0 {
        format!("{:.1} MB/s", s)
    } else {
        format!("{:.0} KB/s", bps as f64 / 1024.0)
    }
}

impl Default for TransferDock {
    fn default() -> Self { Self::new() }
}
