//! Minimal mirror-sync helper (diff only, no auto-sync loop).
//!
//! The daemon's `mirror_diff(device, remote_path)` D-Bus method lists the
//! remote dir and returns JSON `Vec<MirrorEntry>` (regular files only). The
//! GUI deserializes that and calls [`plan_mirror`] with its local dir to get
//! `(to_push, to_pull)`, then enqueues push/pull jobs itself via the usual
//! `enqueue_push` / `enqueue_pull` methods.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One file side of a mirror comparison: base name + byte size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorEntry {
    pub name: String,
    pub size: u64,
}

/// Compare a local dir against a remote listing by file name + size.
///
/// Returns `(to_push, to_pull)` (both sorted):
/// - local-only names -> `to_push`,
/// - remote-only names -> `to_pull`,
/// - same name but different sizes -> listed in **both** (conflict; the GUI
///   picks the direction),
/// - same name and same size -> in sync, listed nowhere.
///
/// Subdirectories in `local_dir` are skipped (flat, non-recursive diff, like
/// the remote side which only sees `listdir` files). An unreadable
/// `local_dir` is treated as empty (everything remote lands in `to_pull`).
pub fn plan_mirror(local_dir: &Path, remote: &[MirrorEntry]) -> (Vec<String>, Vec<String>) {
    let mut local: HashMap<String, u64> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(local_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip subdirectories; mirror is a flat file diff.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    continue;
                }
                local.insert(name, meta.len());
            }
        }
    }
    plan_mirror_maps(&local, remote)
}

/// Map-based core of [`plan_mirror`] (separated for testability).
fn plan_mirror_maps(local: &HashMap<String, u64>, remote: &[MirrorEntry]) -> (Vec<String>, Vec<String>) {
    let remote_map: HashMap<&str, u64> = remote.iter().map(|e| (e.name.as_str(), e.size)).collect();
    let mut to_push = Vec::new();
    let mut to_pull = Vec::new();
    for (name, size) in local {
        match remote_map.get(name.as_str()) {
            None => to_push.push(name.clone()),
            Some(rs) if *rs != *size => to_push.push(name.clone()),
            _ => {}
        }
    }
    for entry in remote {
        match local.get(&entry.name) {
            None => to_pull.push(entry.name.clone()),
            Some(ls) if *ls != entry.size => to_pull.push(entry.name.clone()),
            _ => {}
        }
    }
    to_push.sort();
    to_pull.sort();
    (to_push, to_pull)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_map(pairs: &[(&str, u64)]) -> HashMap<String, u64> {
        pairs.iter().map(|(n, s)| ((*n).to_string(), *s)).collect()
    }

    #[test]
    fn in_sync_files_are_listed_nowhere() {
        let local = local_map(&[("a.jpg", 10), ("b.jpg", 20)]);
        let remote = vec![
            MirrorEntry { name: "a.jpg".into(), size: 10 },
            MirrorEntry { name: "b.jpg".into(), size: 20 },
        ];
        assert_eq!(plan_mirror_maps(&local, &remote), (vec![], vec![]));
    }

    #[test]
    fn missing_sides_land_in_push_or_pull() {
        let local = local_map(&[("only-local", 1)]);
        let remote = vec![MirrorEntry { name: "only-remote".into(), size: 2 }];
        let (push, pull) = plan_mirror_maps(&local, &remote);
        assert_eq!(push, vec!["only-local".to_string()]);
        assert_eq!(pull, vec!["only-remote".to_string()]);
    }

    #[test]
    fn size_mismatch_is_a_conflict_in_both() {
        let local = local_map(&[("same-name", 1)]);
        let remote = vec![MirrorEntry { name: "same-name".into(), size: 2 }];
        let (push, pull) = plan_mirror_maps(&local, &remote);
        assert_eq!(push, vec!["same-name".to_string()]);
        assert_eq!(pull, vec!["same-name".to_string()]);
    }

    #[test]
    fn unreadable_local_dir_treats_everything_as_to_pull() {
        let (push, pull) = plan_mirror(
            Path::new("/definitely/not/a/real/adbshare-mirror-test-dir"),
            &[MirrorEntry { name: "r".into(), size: 1 }],
        );
        assert!(push.is_empty());
        assert_eq!(pull, vec!["r".to_string()]);
    }

    #[test]
    fn reads_local_dir_sizes_from_disk() {
        let base = std::env::temp_dir().join(format!(
            "adbshare-mirror-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("keep"), b"12345").unwrap();
        std::fs::write(base.join("new-local"), b"xy").unwrap();
        let remote = vec![MirrorEntry { name: "keep".into(), size: 5 }];
        let (push, pull) = plan_mirror(&base, &remote);
        assert_eq!(push, vec!["new-local".to_string()]);
        assert!(pull.is_empty());
        std::fs::remove_dir_all(&base).ok();
    }
}
