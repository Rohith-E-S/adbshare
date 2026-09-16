//! Floating bottom transfer dock — a glass-effect bar pinned to the bottom
//! of the window while copies run.  Shows a sync icon, aggregate summary &
//! speed, up to MAX_ROWS mini progress rows, and pause/cancel actions.
//! Hidden entirely when nothing is active so it never obscures content.

use gtk4::prelude::*;

use crate::transfer_view::JobInfo;

/// How many mini rows the dock shows at once; overflow is indicated with
/// a "+N more" label.
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
        // ── Root container ──────────────────────────────────────────────
        // Horizontal strip with generous internal padding for a floating
        // pill-shaped appearance.  The CSS class `transfer-dock` provides
        // the glass background, rounded corners, border glow, and shadow.
        let root = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        root.add_css_class("transfer-dock");
        root.set_halign(gtk4::Align::Center);
        root.set_valign(gtk4::Align::End);
        root.set_margin_start(18);
        root.set_margin_end(18);
        root.set_margin_bottom(36);
        root.set_visible(false);

        // ── Left section: sync icon + summary / speed column ────────────
        let left_section = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        left_section.set_valign(gtk4::Align::Center);
        left_section.set_margin_start(14);
        left_section.set_margin_top(10);
        left_section.set_margin_bottom(10);

        let icon = gtk4::Image::from_icon_name("emblem-synchronizing-symbolic");
        icon.set_pixel_size(22);
        icon.set_valign(gtk4::Align::Center);
        icon.set_opacity(0.85);
        left_section.append(&icon);

        let info_column = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        info_column.set_valign(gtk4::Align::Center);

        let summary_label = gtk4::Label::builder()
            .label("Transferring")
            .xalign(0.0)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .build();
        summary_label.add_css_class("dock-summary");
        info_column.append(&summary_label);

        let speed_label = gtk4::Label::builder().label("0 B/s").xalign(0.0).build();
        speed_label.add_css_class("dock-speed");
        info_column.append(&speed_label);

        left_section.append(&info_column);
        root.append(&left_section);

        // ── Separator 1 ────────────────────────────────────────────────
        let sep1 = gtk4::Separator::new(gtk4::Orientation::Vertical);
        sep1.add_css_class("dock-separator");
        sep1.set_valign(gtk4::Align::Center);
        sep1.set_margin_start(12);
        sep1.set_margin_end(12);
        sep1.set_margin_top(8);
        sep1.set_margin_bottom(8);
        root.append(&sep1);

        // ── Middle section: mini progress rows ──────────────────────────
        let rows_box = gtk4::Box::new(gtk4::Orientation::Vertical, 3);
        rows_box.set_valign(gtk4::Align::Center);
        rows_box.set_hexpand(true);
        rows_box.set_margin_top(8);
        rows_box.set_margin_bottom(8);
        root.append(&rows_box);

        // ── Separator 2 ────────────────────────────────────────────────
        let sep2 = gtk4::Separator::new(gtk4::Orientation::Vertical);
        sep2.add_css_class("dock-separator");
        sep2.set_valign(gtk4::Align::Center);
        sep2.set_margin_start(12);
        sep2.set_margin_end(12);
        sep2.set_margin_top(8);
        sep2.set_margin_bottom(8);
        root.append(&sep2);

        // ── Right section: pause / cancel buttons ───────────────────────
        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        actions.set_valign(gtk4::Align::Center);
        actions.set_margin_end(14);
        actions.set_margin_top(10);
        actions.set_margin_bottom(10);

        let pause_button = gtk4::Button::from_icon_name("media-playback-pause-symbolic");
        pause_button.add_css_class("flat");
        pause_button.add_css_class("circular");
        pause_button.set_tooltip_text(Some("Pause all transfers"));
        actions.append(&pause_button);

        let cancel_button = gtk4::Button::from_icon_name("process-stop-symbolic");
        cancel_button.add_css_class("flat");
        cancel_button.add_css_class("circular");
        cancel_button.add_css_class("destructive");
        cancel_button.set_tooltip_text(Some("Cancel all transfers"));
        actions.append(&cancel_button);

        root.append(&actions);

        Self {
            root,
            pause_button,
            cancel_button,
            summary_label,
            speed_label,
            rows_box,
        }
    }

    /// Rebuild the dock from the current job list.  Running / Pending / Paused
    /// jobs count (paused transfers stay visible and resumable); with none
    /// active the dock hides itself.  `paused` is the global pause toggle and
    /// switches the pause button between "pause" and "resume".
    pub fn update(&self, jobs: &[JobInfo], paused: bool) {
        // ── Collect active jobs ─────────────────────────────────────────
        let active: Vec<&JobInfo> = jobs
            .iter()
            .filter(|j| j.state == "Running" || j.state == "Pending" || j.state == "Paused")
            .collect();

        // ── Toggle pause / resume icon ──────────────────────────────────
        if paused {
            self.pause_button
                .set_icon_name("media-playback-start-symbolic");
            self.pause_button
                .set_tooltip_text(Some("Resume all transfers"));
        } else {
            self.pause_button
                .set_icon_name("media-playback-pause-symbolic");
            self.pause_button
                .set_tooltip_text(Some("Pause all transfers"));
        }

        // ── Hide when idle ──────────────────────────────────────────────
        if active.is_empty() {
            self.root.set_visible(false);
            return;
        }
        self.root.set_visible(true);

        // ── Summary label ───────────────────────────────────────────────
        let n = active.len();
        self.summary_label.set_label(&format!(
            "{} file{} copying",
            n,
            if n == 1 { "" } else { "s" }
        ));

        // ── Aggregate speed ─────────────────────────────────────────────
        let total_speed: u64 = active.iter().map(|j| j.speed_bps).sum();
        self.speed_label.set_label(&human_speed(total_speed));

        // ── Rebuild mini rows ───────────────────────────────────────────
        while let Some(child) = self.rows_box.first_child() {
            self.rows_box.remove(&child);
        }

        for j in active.iter().take(MAX_ROWS) {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.set_valign(gtk4::Align::Center);

            // Direction arrow prefix
            let arrow = match j.direction.as_str() {
                "Push" => "↑ ",
                "Pull" => "↓ ",
                _ => "",
            };

            let name = gtk4::Label::builder()
                .label(format!("{}{}", arrow, j.name))
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .width_chars(22)
                .max_width_chars(22)
                .build();
            name.add_css_class("dock-mini-name");
            row.append(&name);

            // Percentage text
            let pct = if j.bytes_total > 0 {
                j.bytes_done * 100 / j.bytes_total
            } else {
                0
            };
            let pct_lbl = gtk4::Label::new(Some(&format!("{}%", pct)));
            pct_lbl.add_css_class("dock-mini-pct");
            pct_lbl.set_valign(gtk4::Align::Center);
            row.append(&pct_lbl);

            // Gradient-filled progress bar
            let bar = gtk4::ProgressBar::new();
            bar.set_show_text(false);
            let frac = if j.bytes_total > 0 {
                (j.bytes_done as f64 / j.bytes_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            bar.set_fraction(frac);
            bar.set_width_request(120);
            bar.set_valign(gtk4::Align::Center);
            bar.set_hexpand(false);
            row.append(&bar);

            self.rows_box.append(&row);
        }

        // Overflow indicator
        if n > MAX_ROWS {
            let more = gtk4::Label::new(Some(&format!("+{} more", n - MAX_ROWS)));
            more.add_css_class("dock-mini-pct");
            more.set_xalign(0.0);
            self.rows_box.append(&more);
        }
    }
}

/// "84.2 MB/s" style aggregate readout.
fn human_speed(bps: u64) -> String {
    let s = bps as f64 / 1_048_576.0;
    if s >= 1.0 {
        format!("{:.1} MB/s", s)
    } else {
        format!("{:.0} KB/s", bps as f64 / 1024.0)
    }
}

impl Default for TransferDock {
    fn default() -> Self {
        Self::new()
    }
}
