//! Transfer view: a sleek card-based job list with per-job progress.
//!
//! Each transfer is rendered as a rich card widget containing:
//!  • Direction badge (↑ to phone / ↓ to computer)
//!  • File name with ellipsis overflow
//!  • State pill (Copying / Waiting / Paused / Done / Failed / Cancelled)
//!  • Full-width gradient progress bar
//!  • Monospace speed + ETA status line

use gtk4::prelude::*;
use libadwaita as adw;

// ─── Job model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: u64,
    pub direction: String,
    pub name: String,
    pub state: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
    pub eta_secs: u64,
    pub error: Option<String>,
    pub device: Option<String>,
}

impl JobInfo {
    fn status_text(&self) -> String {
        let total = if self.bytes_total == 0 {
            "?".to_string()
        } else {
            human_size(self.bytes_total)
        };
        let done = human_size(self.bytes_done);
        let pct = if self.bytes_total > 0 {
            (self.bytes_done * 100 / self.bytes_total) as u64
        } else {
            0
        };

        let state = match self.state.as_str() {
            "Running" => "Copying",
            "Pending" => "Waiting",
            "Paused" => "Paused",
            "Completed" | "Done" => "Done",
            "Skipped" => "Skipped",
            "Failed" => "Failed",
            "Cancelled" => "Cancelled",
            other => other,
        };

        let mut parts = vec![state.to_string(), format!("{}/{} ({}%)", done, total, pct)];
        if self.state == "Failed" {
            if let Some(error) = self.error.as_deref().filter(|e| !e.is_empty()) {
                parts.push(error.to_string());
            }
        }
        if let Some(device) = self.device.as_deref().filter(|d| !d.is_empty()) {
            parts.push(device.to_string());
        }

        if self.state == "Running" {
            if self.speed_bps > 0 {
                parts.push(format!("{}/s", human_size(self.speed_bps)));
            }
            if self.eta_secs > 0 {
                let mins = self.eta_secs / 60;
                let secs = self.eta_secs % 60;
                if mins > 0 {
                    parts.push(format!("ETA: {}m {}s", mins, secs));
                } else {
                    parts.push(format!("ETA: {}s", secs));
                }
            }
        }

        parts.join(" • ")
    }

    fn fraction(&self) -> f64 {
        if self.bytes_total == 0 {
            return 0.0;
        }
        (self.bytes_done as f64) / (self.bytes_total as f64)
    }
}

// ─── View ───────────────────────────────────────────────────────────────────

pub struct TransferView {
    root: gtk4::Box,
    stack: gtk4::Stack,
    #[allow(dead_code)]
    status: adw::StatusPage,
    list_box: gtk4::ListBox,
}

impl TransferView {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);

        // ── Scrollable job list ─────────────────────────────────────────
        let list_box = gtk4::ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::None);
        list_box.add_css_class("boxed-list");
        // Transparent background so the card styling controls appearance.
        list_box.add_css_class("sidebar-list");

        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();
        scrolled.set_child(Some(&list_box));

        // ── Empty state ─────────────────────────────────────────────────
        let status = adw::StatusPage::builder()
            .title("No transfers yet")
            .description(
                "Send files to the phone or save them to this computer \
                 and they will show up here.",
            )
            .icon_name("emblem-synchronizing-symbolic")
            .vexpand(true)
            .build();

        // ── Stack (list ↔ empty) ────────────────────────────────────────
        let stack = gtk4::Stack::new();
        stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        stack.set_transition_duration(200);
        stack.add_named(&scrolled, Some("list"));
        stack.add_named(&status, Some("empty"));
        stack.set_visible_child_name("empty");
        root.append(&stack);

        Self {
            root,
            stack,
            status,
            list_box,
        }
    }

    pub fn attach(&self, parent: &gtk4::Box) {
        parent.append(&self.root);
    }

    /// Rebuild the entire list from the supplied jobs.
    ///
    /// Jobs are bounded by queue depth so a full rebuild is cheap and avoids
    /// diff-tracking complexity.
    pub fn update_jobs(&self, jobs: Vec<JobInfo>) {
        // Clear existing rows.
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        if jobs.is_empty() {
            self.show_empty();
            return;
        }
        self.show_list();

        for j in &jobs {
            let row = self.build_card(j);
            self.list_box.append(&row);
        }
    }

    // ── Internal builders ───────────────────────────────────────────────

    /// Build a single transfer card row for the given job.
    fn build_card(&self, j: &JobInfo) -> gtk4::ListBoxRow {
        let outer = gtk4::ListBoxRow::new();
        outer.set_activatable(false);
        outer.set_selectable(false);
        outer.add_css_class("transfer-row");

        // Card container — vertical box with padding.
        let card = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        card.add_css_class("transfer-card");

        // ── Top line: direction arrow + file name + state pill ───────
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        top.set_hexpand(true);

        // Direction badge.
        let dir_label = gtk4::Label::new(Some(arrow(&j.direction)));
        dir_label.add_css_class("transfer-dir-badge");
        dir_label.set_valign(gtk4::Align::Center);
        top.append(&dir_label);

        // File name — ellipsize in the middle to keep the extension visible.
        let name_label = gtk4::Label::builder()
            .label(&j.name)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .build();
        name_label.add_css_class("transfer-name");
        name_label.set_use_markup(false);
        top.append(&name_label);

        // State pill.
        let (state_text, state_class) = state_pill(&j.state);
        let state_label = gtk4::Label::new(Some(state_text));
        state_label.add_css_class("transfer-state-pill");
        state_label.add_css_class(state_class);
        state_label.set_valign(gtk4::Align::Center);
        top.append(&state_label);

        card.append(&top);

        // ── Progress bar ────────────────────────────────────────────
        let progress = gtk4::ProgressBar::builder()
            .fraction(j.fraction().clamp(0.0, 1.0))
            .hexpand(true)
            .valign(gtk4::Align::Center)
            .show_text(false)
            .build();
        progress.add_css_class("transfer-progress");
        card.append(&progress);

        // ── Bottom line: monospace status text ──────────────────────
        let status = gtk4::Label::builder()
            .label(&j.status_text())
            .xalign(0.0)
            .build();
        status.add_css_class("transfer-status");
        card.append(&status);

        outer.set_child(Some(&card));
        outer
    }

    fn show_empty(&self) {
        self.stack.set_visible_child_name("empty");
    }

    fn show_list(&self) {
        self.stack.set_visible_child_name("list");
    }
}

impl Default for TransferView {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn arrow(direction: &str) -> &'static str {
    match direction {
        "Push" => "↑ to phone",
        "Pull" => "↓ to computer",
        _ => "•",
    }
}

fn human_size(bytes: u64) -> String {
    let mut s = bytes as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if s < 1024.0 {
            return if unit == "B" {
                format!("{} {}", s as u64, unit)
            } else {
                format!("{:.1} {}", s, unit)
            };
        }
        s /= 1024.0;
    }
    format!("{:.1} P", s)
}

/// Map a job state string to a human-readable label and a CSS modifier class.
fn state_pill(state: &str) -> (&'static str, &'static str) {
    match state {
        "Running" => ("Copying", "pill-running"),
        "Pending" => ("Waiting", "pill-pending"),
        "Paused" => ("Paused", "pill-paused"),
        "Completed" | "Done" => ("Done", "pill-done"),
        "Skipped" => ("Skipped", "pill-done"),
        "Failed" => ("Failed", "pill-failed"),
        "Cancelled" => ("Cancelled", "pill-cancelled"),
        _ => ("Unknown", "pill-pending"),
    }
}
