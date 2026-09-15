//! Minimal photo-import helper (backend only, no GUI).
//!
//! Lists camera directories on the device, skips files already present at
//! the destination (matched by file name + size), and enqueues `Pull` jobs
//! for the new ones into `dest_base/YYYY-MM-DD/` folders (date from the
//! remote `mtime`, UTC).

use std::path::{Path, PathBuf};

use adb_proxy::ProxyClient;
use serde::{Deserialize, Serialize};

use super::job::{Direction, Job, JobId, JobOptions};
use super::queue::JobQueue;

/// Source dirs used when the caller passes an empty `src_dirs`.
pub const DEFAULT_PHOTO_SRC_DIRS: &[&str] = &["/sdcard/DCIM/Camera"];

/// Outcome of [`import_photos`]; serialized to JSON over D-Bus.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhotoImportResult {
    /// Job ids enqueued for new files.
    pub enqueued: Vec<JobId>,
    /// Files skipped because `dest_base/<date>/<name>` already exists with
    /// the same size.
    pub skipped: u64,
    /// Source dirs that could not be listed (missing/unreadable).
    pub missing_src_dirs: Vec<String>,
}

/// Fill in [`DEFAULT_PHOTO_SRC_DIRS`] when the caller passes nothing.
pub fn resolve_src_dirs(src_dirs: &[String]) -> Vec<String> {
    if src_dirs.is_empty() {
        DEFAULT_PHOTO_SRC_DIRS.iter().map(|s| s.to_string()).collect()
    } else {
        src_dirs.to_vec()
    }
}

/// `YYYY-MM-DD` (UTC) for a remote `mtime`; falls back to `unknown-date`.
pub fn import_date_dir(mtime: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(mtime, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown-date".to_string())
}

/// Destination for one remote file: `dest_base/<date>/<file_name>`.
pub fn dest_for_import(dest_base: &Path, mtime: i64, file_name: &str) -> PathBuf {
    dest_base.join(import_date_dir(mtime)).join(file_name)
}

/// List `src_dirs` on the device, skip files already present at `dest_base`
/// (same file name + same size), and enqueue `Pull` jobs for the rest.
///
/// Signature (conceptual): `import_photos(device, src_dirs=[DCIM/Camera],
/// dest_base)`.
pub async fn import_photos(
    client: &ProxyClient,
    queue: &JobQueue,
    device: &str,
    src_dirs: &[String],
    dest_base: &Path,
) -> anyhow::Result<PhotoImportResult> {
    let mut out = PhotoImportResult::default();
    for src_dir in resolve_src_dirs(src_dirs) {
        let entries = match client.listdir(&src_dir).await {
            Ok(e) => e,
            Err(_) => {
                out.missing_src_dirs.push(src_dir);
                continue;
            }
        };
        let prefix = src_dir.trim_end_matches('/');
        for entry in entries {
            if entry.stat.mode.is_dir() {
                continue;
            }
            let dest = dest_for_import(dest_base, entry.stat.mtime, &entry.name);
            if let Ok(meta) = tokio::fs::metadata(&dest).await {
                if meta.len() == entry.stat.size {
                    out.skipped += 1;
                    continue;
                }
            }
            let src = PathBuf::from(format!("{prefix}/{}", entry.name));
            let job = Job::with_device(
                0,
                Direction::Pull,
                src,
                dest,
                JobOptions::default(),
                Some(device.to_string()),
            );
            out.enqueued.push(queue.submit(job));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_dest_uses_utc_day() {
        // 2024-01-15T00:00:00Z
        let dest = dest_for_import(Path::new("/pics"), 1705276800, "IMG_1.jpg");
        assert_eq!(dest, PathBuf::from("/pics/2024-01-15/IMG_1.jpg"));
    }

    #[test]
    fn bad_mtime_falls_back() {
        assert_eq!(import_date_dir(i64::MIN), "unknown-date");
    }

    #[test]
    fn empty_src_dirs_use_default() {
        assert_eq!(resolve_src_dirs(&[]), vec!["/sdcard/DCIM/Camera".to_string()]);
        let custom = vec!["/sdcard/Pictures".to_string()];
        assert_eq!(resolve_src_dirs(&custom), custom);
    }
}
