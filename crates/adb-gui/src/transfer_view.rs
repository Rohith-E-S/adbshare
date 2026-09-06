//! Transfer view: a list of jobs with per-job progress.

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::ActionRowExt;

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
}

impl JobInfo {
    fn status_text(&self) -> String {
        let total = if self.bytes_total == 0 { "?".to_string() } else { human_size(self.bytes_total) };
        let done = human_size(self.bytes_done);
        let pct = if self.bytes_total > 0 {
            (self.bytes_done * 100 / self.bytes_total) as u64
        } else { 0 };

        let state = match self.state.as_str() {
            "Running" => "Copying",
            "Pending" => "Waiting",
            "Paused" => "Paused",
            "Done" => "Done",
            "Failed" => "Failed",
            "Cancelled" => "Cancelled",
            other => other,
        };
        let mut parts = vec![
            state.to_string(),
            format!("{}/{} ({}%)", done, total, pct),
        ];

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
        if self.bytes_total == 0 { return 0.0; }
        (self.bytes_done as f64) / (self.bytes_total as f64)
    }
}

pub struct TransferView {
    root: gtk4::Box,
    stack: gtk4::Stack,
    #[allow(dead_code)] status: adw::StatusPage,
    list_box: gtk4::ListBox,
}

impl TransferView {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

        let list_box = gtk4::ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::None);
        let scrolled = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .build();
        scrolled.set_child(Some(&list_box));

        let status = adw::StatusPage::builder()
            .title("No transfers yet")
            .description("Send files to the phone or save them to this computer and they will show up here.")
            .icon_name("emblem-synchronizing-symbolic")
            .vexpand(true)
            .build();

        let stack = gtk4::Stack::new();
        stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
        stack.add_named(&scrolled, Some("list"));
        stack.add_named(&status, Some("empty"));
        stack.set_visible_child_name("empty");
        root.append(&stack);

        Self { root, stack, status, list_box }
    }

    pub fn attach(&self, parent: &gtk4::Box) {
        parent.append(&self.root);
    }

    /// Used when the caller wants to add a header label above the transfer
    /// list: they own the wrapping box and call this on it.
    pub fn transfer_attach(&self, parent: &gtk4::Box) {
        parent.append(&self.root);
    }

    pub fn root_box(&self) -> &gtk4::Box { &self.root }

    pub fn update_jobs(&self, jobs: Vec<JobInfo>) {
        // Always rebuild: jobs are bounded (queue depth) and this avoids
        // diffing complexity.
        while let Some(child) = self.list_box.first_child() {
            self.list_box.remove(&child);
        }

        if jobs.is_empty() {
            self.show_empty();
            return;
        }
        self.show_list();

        for j in jobs {
            let row = adw::ActionRow::builder()
                .title(format!("{} {}", arrow(&j.direction), j.name))
                .subtitle(&j.status_text())
                .build();
            row.add_css_class("transfer-card");
            let progress = gtk4::ProgressBar::builder()
                .fraction(j.fraction().clamp(0.0, 1.0))
                .valign(gtk4::Align::Center)
                .build();
            progress.set_show_text(false);
            progress.set_hexpand(true);
            progress.set_valign(gtk4::Align::End);
            row.add_suffix(&progress);
            self.list_box.append(&row);
        }
    }

    fn show_empty(&self) {
        self.stack.set_visible_child_name("empty");
    }
    fn show_list(&self) {
        self.stack.set_visible_child_name("list");
    }
}

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
            return if unit == "B" { format!("{} {}", s as u64, unit) }
                    else { format!("{:.1} {}", s, unit) };
        }
        s /= 1024.0;
    }
    format!("{:.1} P", s)
}

impl Default for TransferView {
    fn default() -> Self { Self::new() }
}