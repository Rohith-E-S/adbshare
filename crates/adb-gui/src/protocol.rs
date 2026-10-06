//! Wire types and the in-memory models the UI renders.
//!
//! The daemon speaks D-Bus, but every structured payload is a JSON string, so
//! these DTOs are the deserialisation boundary. They mirror the copies in
//! `adb-daemon`; if you change one side, change both.
//!
//! Presentation helpers (`human_size`, `format_capacity`, `format_diagnostics`)
//! live here too so the views stay free of formatting logic and so the existing
//! unit tests have somewhere obvious to reach.

use std::path::{Path, PathBuf};

use gpui::Global;
use serde::{Deserialize, Serialize};

/// Sentinel "serial" used when the browser is pointed at the local disk.
pub const LOCAL_DEVICE: &str = "__local__";

// ── Models ───────────────────────────────────────────────────────────────────

/// One row in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
}

impl DirEntry {
    /// True for anything a folder-drawing icon should be used for.
    ///
    /// `is_dir` alone, and deliberately not `is_dir || is_symlink`. Both entry
    /// producers already resolve a symlink's target: the daemon stats anything
    /// that comes back as a symlink and fills in `is_dir`, and local mode uses
    /// `fs::metadata`, which follows. So a symlink to a directory arrives with
    /// `is_dir = true`, and the only thing `is_symlink` added was the false
    /// positive: a symlink to a *file* was treated as a folder, so it drew a
    /// folder icon, double-click tried to list it and reported "Could not open
    /// folder", Ctrl+U enqueued a tree push, and Save-to-computer took the
    /// `copy_tree` path and failed with ENOTDIR.
    pub fn looks_like_dir(&self) -> bool {
        self.is_dir
    }

    /// Lowercase extension, empty when there is none.
    pub fn ext(&self) -> String {
        Path::new(&self.name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// Permission bits rendered the way `ls -l` would.
    pub fn mode_string(&self) -> String {
        let bits = if self.mode & 0o111 != 0 { 'x' } else { '-' };
        format!(
            "{}{}{}{}{}{}{}{}{}",
            if self.mode & 0o400 != 0 { 'r' } else { '-' },
            if self.mode & 0o200 != 0 { 'w' } else { '-' },
            if self.mode & 0o100 != 0 { bits } else { '-' },
            if self.mode & 0o040 != 0 { 'r' } else { '-' },
            if self.mode & 0o020 != 0 { 'w' } else { '-' },
            if self.mode & 0o010 != 0 { bits } else { '-' },
            if self.mode & 0o004 != 0 { 'r' } else { '-' },
            if self.mode & 0o002 != 0 { 'w' } else { '-' },
            if self.mode & 0o001 != 0 { bits } else { '-' },
        )
    }

    /// Modification time as `YYYY-MM-DD HH:MM`, or an em dash if unknown.
    pub fn mtime_string(&self) -> String {
        if self.mtime <= 0 {
            return "—".into();
        }
        let Some(secs) = chrono::DateTime::from_timestamp(self.mtime, 0) else {
            return "—".into();
        };
        secs.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// A connected device, as reported by `list_devices` + `device_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    pub serial: String,
    pub model: Option<String>,
    /// `"usb"` or `"wifi"`.
    pub transport: &'static str,
    /// `(used, total)` bytes on `/sdcard`, when the device reported them.
    pub storage: Option<(u64, u64)>,
    pub battery_pct: Option<u8>,
}

impl DeviceEntry {
    pub fn display_name(&self) -> &str {
        self.model.as_deref().unwrap_or(&self.serial)
    }

    pub fn transport_label(&self) -> &'static str {
        if self.transport == "wifi" {
            "Wi-Fi"
        } else {
            "USB"
        }
    }

    /// The entry to show for a serial we could not get metadata for.
    pub fn fallback(serial: &str) -> Self {
        Self {
            serial: serial.to_string(),
            model: None,
            transport: "usb",
            storage: None,
            battery_pct: None,
        }
    }
}

/// One transfer job.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// True while the job is queued or running, which is what the status bar's
    /// pause and cancel buttons act on.
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "Pending" | "Running")
    }

    /// Completion in `0.0..=1.0` for the progress bar.
    pub fn fraction(&self) -> f32 {
        if self.bytes_total == 0 {
            return 0.0;
        }
        ((self.bytes_done as f64) / (self.bytes_total as f64)).clamp(0.0, 1.0) as f32
    }

    /// Human label for the state pill: `(text, semantic tone)`.
    pub fn state_pill(&self) -> (&'static str, StateTone) {
        match self.state.as_str() {
            "Running" => ("Copying", StateTone::Active),
            "Pending" => ("Waiting", StateTone::Neutral),
            "Paused" => ("Paused", StateTone::Warning),
            "Completed" | "Done" => ("Done", StateTone::Success),
            "Skipped" => ("Skipped", StateTone::Success),
            "Failed" => ("Failed", StateTone::Danger),
            "Cancelled" => ("Cancelled", StateTone::Neutral),
            _ => ("Unknown", StateTone::Neutral),
        }
    }

    /// Arrow and caption for the direction badge.
    pub fn direction_badge(&self) -> (&'static str, &'static str) {
        match self.direction.as_str() {
            "Push" => ("↑", "to phone"),
            "Pull" => ("↓", "to computer"),
            _ => ("•", ""),
        }
    }

    /// The monospace status line under the progress bar.
    pub fn status_text(&self) -> String {
        let total = if self.bytes_total == 0 {
            "?".to_string()
        } else {
            human_size(self.bytes_total)
        };
        let done = human_size(self.bytes_done);
        let pct = (self.fraction() * 100.0).round() as u64;

        let (state, _) = self.state_pill();
        let mut parts = vec![state.to_string(), format!("{done}/{total} ({pct}%)")];

        if self.state == "Failed"
            && let Some(error) = self.error.as_deref().filter(|e| !e.is_empty())
        {
            parts.push(error.to_string());
        }
        if let Some(device) = self.device.as_deref().filter(|d| !d.is_empty()) {
            parts.push(device.to_string());
        }

        if self.state == "Running" {
            if self.speed_bps > 0 {
                parts.push(format!("{}/s", human_size(self.speed_bps)));
            }
            if self.eta_secs > 0 {
                let (mins, secs) = (self.eta_secs / 60, self.eta_secs % 60);
                if mins > 0 {
                    parts.push(format!("ETA: {mins}m {secs}s"));
                } else {
                    parts.push(format!("ETA: {secs}s"));
                }
            }
        }

        parts.join(" • ")
    }
}

/// Semantic tone for a state pill, resolved to a palette colour at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTone {
    Active,
    Neutral,
    Success,
    Warning,
    Danger,
}

/// Which side of a transfer the clipboard snapshot came from.
#[derive(Debug, Clone)]
pub struct ClipboardFiles {
    /// True when copied from the local view; false when copied from a device.
    pub from_local: bool,
    /// Device serial when `from_local` is false.
    pub device: Option<String>,
    /// Directory that was being browsed when the copy happened.
    pub from_dir: PathBuf,
    /// Copied entries. Directories paste through recursive tree operations.
    pub entries: Vec<DirEntry>,
}

// ── DTOs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceInfoDto {
    pub serial: String,
    #[serde(default)]
    pub model: Option<String>,
    pub transport: String,
    #[serde(default)]
    pub battery_pct: Option<u8>,
    #[serde(default)]
    pub storage_used: Option<u64>,
    #[serde(default)]
    pub storage_total: Option<u64>,
}

impl DeviceInfoDto {
    pub fn into_entry(self) -> DeviceEntry {
        DeviceEntry {
            serial: self.serial,
            model: self.model,
            transport: if self.transport == "wifi" {
                "wifi"
            } else {
                "usb"
            },
            storage: match (self.storage_used, self.storage_total) {
                (Some(used), Some(total)) if total > 0 => Some((used, total)),
                _ => None,
            },
            battery_pct: self.battery_pct,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntryDto {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
}

impl From<DirEntryDto> for DirEntry {
    fn from(d: DirEntryDto) -> Self {
        DirEntry {
            name: d.name,
            is_dir: d.is_dir,
            is_symlink: d.is_symlink,
            size: d.size,
            mode: d.mode,
            mtime: d.mtime,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDto {
    pub id: u64,
    pub direction: String,
    pub source: String,
    pub destination: String,
    pub state: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    #[serde(default)]
    pub speed_bps: u64,
    #[serde(default)]
    pub eta_secs: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
}

impl From<JobDto> for JobInfo {
    fn from(j: JobDto) -> Self {
        let name = Path::new(&j.source)
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TreeEnqueueResult {
    #[serde(default)]
    pub enqueued: Vec<u64>,
    /// Files `copy_tree` copied on the device without going through the queue,
    /// so it has no job ids. Counted separately rather than reported as
    /// invented ids, which used to collide with real queue ids.
    #[serde(default)]
    pub copied: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceDiagDto {
    pub serial: String,
    #[serde(default)]
    pub mounted: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DiagnosticReportDto {
    #[serde(default)]
    pub adb_ok: bool,
    #[serde(default)]
    pub adb_version: String,
    #[serde(default)]
    pub adb_server: String,
    #[serde(default)]
    pub mount_base: String,
    #[serde(default)]
    pub proxy_conns: usize,
    #[serde(default)]
    pub no_fuse: bool,
    #[serde(default)]
    pub helper_env_present: bool,
    #[serde(default)]
    pub helper_env_exists: bool,
    #[serde(default)]
    pub auth_key_path: String,
    #[serde(default)]
    pub devices: Vec<DeviceDiagDto>,
}

/// Render a diagnostics report as the plain text the "Copy report" button puts
/// on the clipboard.
pub fn format_diagnostics(report: &DiagnosticReportDto) -> String {
    let mut lines = Vec::new();
    if report.adb_ok {
        lines.push(format!("ADB: found ({})", report.adb_version));
    } else {
        lines.push("ADB: not found on PATH — install android-tools and retry.".to_string());
    }
    lines.push(format!("ADB server: {}", report.adb_server));
    if !report.auth_key_path.is_empty() {
        lines.push(format!("Auth key: {}", report.auth_key_path));
    }
    if report.helper_env_present && report.helper_env_exists {
        lines.push("Phone helper: ADBSHARE_PROXY_BIN points at a file.".to_string());
    } else if report.helper_env_present {
        lines.push("Phone helper: ADBSHARE_PROXY_BIN is set but the file is missing — rebuild the ARM64 helper.".to_string());
    } else {
        lines.push("Phone helper: ADBSHARE_PROXY_BIN is not set — the daemon falls back to source-tree builds.".to_string());
    }
    lines.push(format!(
        "Mounts: {} (base {}, {} connections per device)",
        if report.no_fuse {
            "disabled"
        } else {
            "enabled"
        },
        report.mount_base,
        report.proxy_conns,
    ));
    if report.devices.is_empty() {
        lines.push("Devices: none ready — connect a phone, unlock it, and accept the USB debugging prompt.".to_string());
    } else {
        for device in &report.devices {
            lines.push(format!(
                "Device {}: {}",
                device.serial,
                if device.mounted {
                    "ready, mounted"
                } else {
                    "ready, FUSE mount unavailable — use in-app browsing"
                },
            ));
        }
    }
    lines.join("\n")
}

/// Summarise a tree enqueue for a result dialog.
///
/// Returns `Err` only when nothing was queued *and* something failed; a partial
/// success is reported as `Ok` with the failures appended, because the user
/// still needs to know what did not transfer.
pub fn tree_result_message(action: &str, result: &TreeEnqueueResult) -> Result<String, String> {
    // `copy_tree` copies on the device and never queues, so total the two.
    let total = result.enqueued.len() + result.copied;
    if total == 0 && !result.errors.is_empty() {
        return Err(result.errors.join("\n"));
    }
    let mut message = format!("{action} queued {total} file(s)");
    if !result.errors.is_empty() {
        message.push_str("\n\nSome entries failed:\n");
        message.push_str(&result.errors.join("\n"));
    }
    Ok(message)
}

// ── Transfer policy ──────────────────────────────────────────────────────────

/// Conflict handling and checksum verification applied to new transfers.
///
/// The GTK build kept these in two `OnceLock<Mutex<..>>` statics; a [`Global`]
/// is the same idea with somewhere to live that the rest of the app can read.
pub struct TransferPolicy {
    /// One of `skip`, `replace`, `keep-both`.
    pub overwrite: String,
    pub verify: bool,
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            overwrite: "skip".into(),
            verify: false,
        }
    }
}

impl Global for TransferPolicy {}

// ── Formatting ───────────────────────────────────────────────────────────────

/// `1536` becomes `1.5 K`.
pub fn human_size(bytes: u64) -> String {
    let mut s = bytes as f64;
    for unit in ["B", "K", "M", "G", "T"] {
        if s < 1024.0 {
            return if unit == "B" {
                format!("{} {unit}", s as u64)
            } else {
                format!("{s:.1} {unit}")
            };
        }
        s /= 1024.0;
    }
    format!("{s:.1} P")
}

/// `(used, total)` becomes `23.4 of 128 GB used`, switching to TB when needed.
pub fn format_capacity((used, total): (u64, u64)) -> String {
    let (unit, div) = if total as f64 >= 1024.0_f64.powi(4) {
        ("TB", 1024.0_f64.powi(4))
    } else {
        ("GB", 1024.0_f64.powi(3))
    };
    format!(
        "{:.1} of {:.0} {unit} used",
        used as f64 / div,
        total as f64 / div
    )
}

/// Case-insensitive substring test that does not allocate for ASCII input.
///
/// The search box runs this over every entry on every keystroke, so
/// `name.to_lowercase().contains(..)` cost one String allocation per entry per
/// keystroke — 5,000 allocations in a large folder.
///
/// ASCII, which is the overwhelming majority of Android filenames, is compared
/// byte-wise with no allocation. Anything else falls back to allocating, because
/// doing Unicode case folding without a table is not worth the complexity for the
/// minority of names that need it.
///
/// `needle` must already be lowercase; the search field lowercases it once.
pub fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        let hay = haystack.as_bytes();
        let ned = needle.as_bytes();
        if ned.len() > hay.len() {
            return false;
        }
        return hay
            .windows(ned.len())
            .any(|window| window.eq_ignore_ascii_case(ned));
    }
    haystack.to_lowercase().contains(needle)
}

/// What a listing is ordered by.
///
/// Serialised by its stable lower-case name rather than as a variant index, so
/// reordering the enum cannot silently reinterpret a saved preference. An
/// unrecognised name deserialises to the default rather than failing, so a
/// preferences file written by a newer build — or hand-edited — does not cost
/// the user every other setting in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Modified,
}

impl SortKey {
    /// The menu item id, which is also the preference name.
    pub fn id(self) -> &'static str {
        self.as_name()
    }

    /// The label shown in the sort menu.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Size => "Size",
            SortKey::Modified => "Modified",
        }
    }

    /// Every key, in menu order.
    pub fn all() -> [SortKey; 3] {
        [SortKey::Name, SortKey::Size, SortKey::Modified]
    }

    /// Every key, for iterating without importing the enum's variants.
    pub fn values() -> impl Iterator<Item = Self> {
        Self::all().into_iter()
    }

    /// The `SortKey` a persisted string names, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "name" => Some(SortKey::Name),
            "size" => Some(SortKey::Size),
            "modified" => Some(SortKey::Modified),
            _ => None,
        }
    }

    /// The stable name used in the preferences file.
    pub fn as_name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
        }
    }
}

impl<'de> serde::Deserialize<'de> for SortKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(SortKey::from_name(&name).unwrap_or_default())
    }
}

/// Order directory entries.
///
/// Folders always come first — every file manager does it, the daemon's own
/// listings already do it, and changing it would be a regression. Within a
/// group the entries are ordered by `key`, optionally reversed, with names folded
/// to lowercase so ordering is case-insensitive.
///
/// Built as decorate-sort-undecorate rather than a custom comparator: a
/// comparator that folds both names allocates twice per comparison, which for
/// 5,000 entries is well over a hundred thousand allocations for one sort.
///
/// Returns the permutation that was applied: `order[i]` is the index in the
/// pre-sort slice now living at position `i`. Callers holding indices into
/// `entries` (the browser's `visible`, `selection` and `focused`) need this to
/// remap them, otherwise a sort silently repoints them at different files.
pub fn sort_entries(entries: &mut Vec<DirEntry>, key: SortKey, descending: bool) -> Vec<usize> {
    let mut keyed: Vec<(bool, SortField, usize)> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.is_dir, sort_field(entry, key), index))
        .collect();

    // Stable, so entries that compare equal keep their original relative order.
    // Only the in-group comparison is reversed: flipping the folder comparison
    // too would bury the directories at the bottom, which is never what
    // "reverse order" means.
    keyed.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            let ordering = a.1.cmp(&b.1);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        })
    });
    let order: Vec<usize> = keyed.into_iter().map(|(_, _, index)| index).collect();
    apply_permutation(entries, order.iter().copied());
    order
}

/// The comparable sort field for one entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SortField {
    /// Case-folded name, so `apple` and `Banana` order as a person expects.
    Name(String),
    Size(u64),
    Modified(i64),
}

fn sort_field(entry: &DirEntry, key: SortKey) -> SortField {
    match key {
        SortKey::Name => SortField::Name(entry.name.to_lowercase()),
        SortKey::Size => SortField::Size(entry.size),
        SortKey::Modified => SortField::Modified(entry.mtime),
    }
}

/// Reorder `entries` into the given sequence of original indices.
///
/// Implemented by draining into slots and refilling, which is correct for any
/// permutation and needs no placeholder value.
fn apply_permutation(entries: &mut Vec<DirEntry>, order: impl Iterator<Item = usize>) {
    let mut slots: Vec<Option<DirEntry>> = entries.drain(..).map(Some).collect();
    entries.extend(order.map(|index| {
        slots[index]
            .take()
            .expect("a permutation visits every index once")
    }));
}
