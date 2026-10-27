//! History of what has been done to a phone, persisted under
//! `~/.config/adbshare/`.
//!
//! Two ledgers live here. `install_history.json` records finished installs;
//! `debloat_ledger.json` records every change made to a package's state.
//! Both are JSON arrays, newest last, capped, and written by a
//! read-modify-write: one change per interaction is far below any write
//! frequency that would justify a real store, and the files stay legible.
//!
//! The debloat ledger is what makes the changes reversible. Every action it
//! records has an opposite — a disabled app can be enabled, an unlinked one
//! can be linked back — so the ledger is also the recipe for undoing a
//! session's worth of work in one go.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The most recent installs the file keeps. Older ones fall off the front.
const HISTORY_CAP: usize = 200;

/// The most recent package changes the ledger keeps.
const LEDGER_CAP: usize = 2000;

/// One finished install attempt, success or failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    /// RFC 3339 timestamp of when the attempt finished.
    pub when: String,
    pub package: String,
    #[serde(default)]
    pub version: Option<String>,
    /// Where the APK came from: the file name, or a device path.
    pub source: String,
    pub device: String,
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// One change to a package's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebloatRecord {
    /// RFC 3339 timestamp of when the change was made.
    pub when: String,
    /// The phone it was made on.
    pub device: String,
    /// The Android user it was made for.
    #[serde(default)]
    pub user: u16,
    /// The package that was changed.
    pub package: String,
    /// What was done: `disable`, `enable`, `uninstall` or `restore`.
    pub action: String,
    /// The state before the change.
    #[serde(default)]
    pub before: String,
    /// The state after it.
    #[serde(default)]
    pub after: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// Where the history lives: the same directory the preferences use.
fn history_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("adbshare").join("install_history.json"))
}

/// Read every stored record. A missing file is simply empty history.
pub fn load() -> Vec<InstallRecord> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Append one record and trim to the cap. Best effort: history that cannot be
/// written must not make an install look like it failed.
pub fn record(rec: InstallRecord) {
    let Some(path) = history_path() else {
        return;
    };
    let mut records = load();
    records.push(rec);
    if records.len() > HISTORY_CAP {
        records.drain(..records.len() - HISTORY_CAP);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&records) {
        let _ = std::fs::write(&path, text);
    }
}

/// Where the debloat ledger lives: beside the install history.
fn ledger_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("adbshare").join("debloat_ledger.json"))
}

/// Read every package change on record. A missing file is an empty ledger.
pub fn load_ledger() -> Vec<DebloatRecord> {
    let Some(path) = ledger_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Append one change and trim to the cap.
///
/// Best effort: a ledger that cannot be written must not make a change look
/// like it failed.
pub fn record_change(rec: DebloatRecord) {
    let Some(path) = ledger_path() else {
        return;
    };
    let mut records = load_ledger();
    records.push(rec);
    if records.len() > LEDGER_CAP {
        records.drain(..records.len() - LEDGER_CAP);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&records) {
        let _ = std::fs::write(&path, text);
    }
}

/// The changes on one phone that can be taken back.
///
/// A change can be undone when it worked, and when there is a way back from
/// the state it left the package in: a disabled app can be enabled, an
/// unlinked one can be linked back. The result is the newest change first,
/// so undoing in order undoes the last thing done first.
pub fn undoable(device: &str, user: u16) -> Vec<DebloatRecord> {
    let mut out: Vec<DebloatRecord> = load_ledger()
        .into_iter()
        .filter(|r| r.device == device && r.user == user && r.ok)
        .collect();
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_record() {
        let rec = InstallRecord {
            when: "2026-10-16T12:00:00Z".into(),
            package: "com.example.app".into(),
            version: Some("1.2.3".into()),
            source: "app.apk".into(),
            device: "emulator-5554".into(),
            success: true,
            error: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: InstallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.package, "com.example.app");
        assert!(back.success);
    }

    #[test]
    fn a_ledger_record_round_trips() {
        let rec = DebloatRecord {
            when: "2026-10-21T12:00:00Z".into(),
            device: "emulator-5554".into(),
            user: 0,
            package: "com.samsung.android.bixby.service".into(),
            action: "disable".into(),
            before: "enabled".into(),
            after: "disabled".into(),
            ok: true,
            error: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: DebloatRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.package, "com.samsung.android.bixby.service");
        assert_eq!(back.action, "disable");
        assert_eq!(back.before, "enabled");
        assert!(back.ok);
    }
}
