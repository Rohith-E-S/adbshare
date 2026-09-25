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
    pub fn looks_like_dir(&self) -> bool {
        self.is_dir || self.is_symlink
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
        let pct = self
            .bytes_total
            .checked_div(1)
            .map(|total| self.bytes_done * 100 / total)
            .unwrap_or(0);

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
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceDiagDto {
    pub serial: String,
    #[serde(default)]
    pub setup_ok: bool,
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
                if device.setup_ok {
                    if device.mounted {
                        "ready, mounted"
                    } else {
                        "ready, FUSE mount unavailable — use in-app browsing"
                    }
                } else {
                    "setup incomplete — check the helper build and daemon log"
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
    if result.enqueued.is_empty() && !result.errors.is_empty() {
        return Err(result.errors.join("\n"));
    }
    let mut message = format!("{action} queued {} file(s)", result.enqueued.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(state: &str) -> JobInfo {
        JobInfo {
            id: 1,
            direction: "Push".into(),
            name: "a.bin".into(),
            state: state.into(),
            bytes_done: 512,
            bytes_total: 1024,
            speed_bps: 0,
            eta_secs: 0,
            error: None,
            device: None,
        }
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 K");
        assert_eq!(human_size(1536), "1.5 K");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 M");
    }

    #[test]
    fn state_pill_labels_match_the_old_widget() {
        assert_eq!(job("Running").state_pill().0, "Copying");
        assert_eq!(job("Pending").state_pill().0, "Waiting");
        assert_eq!(job("Completed").state_pill().0, "Done");
        assert_eq!(job("Done").state_pill().0, "Done");
        assert_eq!(job("Skipped").state_pill().0, "Skipped");
        assert_eq!(job("Nonsense").state_pill().0, "Unknown");
    }

    #[test]
    fn status_text_reports_progress_and_failure() {
        assert!(job("Running").status_text().contains("512 B/1.0 K (50%)"));

        let mut failed = job("Failed");
        failed.error = Some("disk full".into());
        failed.device = Some("pixel".into());
        let text = failed.status_text();
        assert!(text.contains("disk full"), "{text}");
        assert!(text.contains("pixel"), "{text}");
    }

    #[test]
    fn fraction_clamps_and_tolerates_zero_total() {
        let mut j = job("Running");
        j.bytes_total = 0;
        assert_eq!(j.fraction(), 0.0);
        j.bytes_total = 1024;
        j.bytes_done = 2048;
        assert_eq!(j.fraction(), 1.0);
    }

    #[test]
    fn job_dto_preserves_error_and_device() {
        let with = JobDto {
            id: 7,
            direction: "Pull".into(),
            source: "/sdcard/Download/report.pdf".into(),
            destination: "/home/me/report.pdf".into(),
            state: "Failed".into(),
            bytes_done: 0,
            bytes_total: 10,
            speed_bps: 0,
            eta_secs: 0,
            error: Some("boom".into()),
            device: Some("pixel".into()),
        };
        let info = JobInfo::from(with);
        assert_eq!(info.name, "report.pdf", "name comes from the source path");
        assert_eq!(info.error.as_deref(), Some("boom"));
        assert_eq!(info.device.as_deref(), Some("pixel"));

        let without = JobDto {
            error: None,
            device: None,
            source: "plain".into(),
            ..serde_json::from_str::<JobDto>(
                r#"{"id":1,"direction":"Push","source":"plain","destination":"d",
                    "state":"Running","bytes_done":0,"bytes_total":0}"#,
            )
            .expect("fixture parses")
        };
        let info = JobInfo::from(without);
        assert_eq!(info.name, "plain");
        assert!(info.error.is_none());
        assert!(info.device.is_none());
    }

    #[test]
    fn device_info_dto_prefers_wifi_and_drops_empty_storage() {
        let dto: DeviceInfoDto = serde_json::from_str(
            r#"{"serial":"abc","model":"Pixel 7","transport":"wifi",
                "battery_pct":77,"storage_used":5,"storage_total":0}"#,
        )
        .expect("fixture parses");
        let entry = dto.into_entry();
        assert_eq!(entry.transport, "wifi");
        assert_eq!(entry.transport_label(), "Wi-Fi");
        assert_eq!(entry.display_name(), "Pixel 7");
        assert_eq!(entry.storage, None, "a zero total means 'unknown'");

        let bare: DeviceInfoDto =
            serde_json::from_str(r#"{"serial":"xyz","transport":"usb"}"#).expect("fixture parses");
        let entry = bare.into_entry();
        assert_eq!(entry.transport, "usb");
        assert_eq!(entry.display_name(), "xyz");
        assert!(entry.battery_pct.is_none());
    }

    #[test]
    fn tree_result_message_separates_total_failure_from_partial() {
        let all_bad = TreeEnqueueResult {
            enqueued: vec![],
            errors: vec!["no space".into()],
        };
        assert_eq!(
            tree_result_message("Upload", &all_bad),
            Err("no space".to_string())
        );

        let partial = TreeEnqueueResult {
            enqueued: vec![1, 2],
            errors: vec!["skipped link".into()],
        };
        let msg = tree_result_message("Upload", &partial).expect("partial success is still Ok");
        assert!(msg.starts_with("Upload queued 2 file(s)"), "{msg}");
        assert!(msg.contains("skipped link"), "{msg}");

        let clean = TreeEnqueueResult {
            enqueued: vec![9],
            errors: vec![],
        };
        assert_eq!(
            tree_result_message("Upload", &clean),
            Ok("Upload queued 1 file(s)".to_string())
        );
    }

    #[test]
    fn diagnostics_cover_missing_adb_and_unready_device() {
        let missing: DiagnosticReportDto = serde_json::from_str(
            r#"{"adb_ok":false,"adb_server":"127.0.0.1:5037","no_fuse":true,
                "devices":[{"serial":"abc","setup_ok":false,"mounted":false}]}"#,
        )
        .expect("fixture parses");
        let text = format_diagnostics(&missing);
        assert!(text.contains("ADB: not found on PATH"), "{text}");
        assert!(text.contains("Mounts: disabled"), "{text}");
        assert!(text.contains("setup incomplete"), "{text}");

        let ready: DiagnosticReportDto = serde_json::from_str(
            r#"{"adb_ok":true,"adb_version":"1.0.41","helper_env_present":true,
                "helper_env_exists":true,"proxy_conns":4,
                "devices":[{"serial":"abc","setup_ok":true,"mounted":true}]}"#,
        )
        .expect("fixture parses");
        let text = format_diagnostics(&ready);
        assert!(text.contains("ADB: found (1.0.41)"), "{text}");
        assert!(text.contains("points at a file"), "{text}");
        assert!(text.contains("ready, mounted"), "{text}");
        assert!(text.contains("4 connections per device"), "{text}");
    }

    #[test]
    fn dir_entry_derives_extension_and_mode() {
        let e = DirEntry {
            name: "archive.TAR.GZ".into(),
            is_dir: false,
            is_symlink: false,
            size: 0,
            mode: 0o755,
            mtime: 0,
        };
        assert_eq!(e.ext(), "gz");
        assert_eq!(e.mode_string(), "rwxr-xr-x");
        assert_eq!(e.mtime_string(), "—");
        assert!(!e.looks_like_dir());

        let link = DirEntry {
            name: "link".into(),
            is_dir: false,
            is_symlink: true,
            size: 0,
            mode: 0o777,
            mtime: 0,
        };
        assert!(link.looks_like_dir(), "a symlink may point at a folder");
    }

    #[test]
    fn format_capacity_picks_gb_or_tb() {
        assert_eq!(
            format_capacity((23 * 1024 * 1024 * 1024, 128 * 1024 * 1024 * 1024)),
            "23.0 of 128 GB used"
        );
        assert_eq!(
            format_capacity((2 * 1024 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024 * 1024)),
            "2.0 of 4 TB used"
        );
    }
}
