//! TTL-based caches for the FUSE layer: stats, keyed by canonical path, and
//! directory listings, keyed the same way.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use adb_proxy::{DirEntry, Stat};

#[cfg(test)]
use adb_proxy::ops::FileMode;

/// Upper bound on cached entries. When exceeded, expired entries are
/// dropped first; if the cache is still over the cap, it is cleared
/// outright (entries only live for the TTL anyway, so a full clear is
/// cheap and bounded growth is what matters).
const MAX_ENTRIES: usize = 8192;

#[derive(Debug)]
pub struct StatCache {
    inner: parking_lot::Mutex<HashMap<PathBuf, (Stat, Instant)>>,
    ttl: Duration,
}

impl StatCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: parking_lot::Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub fn get(&self, path: &PathBuf) -> Option<Stat> {
        let mut inner = self.inner.lock();
        if let Some((s, when)) = inner.get(path) {
            if when.elapsed() < self.ttl {
                return Some(*s);
            }
        } else {
            return None;
        }
        // Expired: drop it so the map does not grow without bound.
        inner.remove(path);
        None
    }

    pub fn put(&self, path: PathBuf, stat: Stat) {
        let mut inner = self.inner.lock();
        if inner.len() >= MAX_ENTRIES {
            let ttl = self.ttl;
            inner.retain(|_, (_, when)| when.elapsed() < ttl);
            if inner.len() >= MAX_ENTRIES {
                inner.clear();
            }
        }
        inner.insert(path, (stat, Instant::now()));
    }

    pub fn invalidate(&self, path: &PathBuf) {
        self.inner.lock().remove(path);
    }

    pub fn invalidate_prefix(&self, prefix: &PathBuf) {
        let mut inner = self.inner.lock();
        inner.retain(|k, _| !k.starts_with(prefix));
    }
}

/// Cached directory listings.
///
/// FUSE delivers a directory in as many `readdir` calls as the consumer needs,
/// each with a continuation offset. Without this, walking a directory with
/// 5,000 entries re-issued `listdir` to the phone once per call — a full
/// directory enumeration over ADB, repeated. Caching the last few listings
/// turns that into one round trip.
///
/// Entries are held briefly and dropped as soon as anything under them is
/// mutated, so the only cost of a stale hit is bounded by the TTL, exactly as
/// for the stat cache.
#[derive(Debug)]
pub struct DirCache {
    inner: parking_lot::Mutex<DirCacheInner>,
    ttl: Duration,
    /// How many directories to remember. Small on purpose: it covers a file
    /// manager walking a tree, without letting a long session grow without
    /// bound.
    capacity: usize,
}

#[derive(Debug, Default)]
struct DirCacheInner {
    map: HashMap<PathBuf, (Vec<DirEntry>, Instant)>,
    /// Insertion order, for evicting the oldest when full.
    order: VecDeque<PathBuf>,
}

impl DirCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: parking_lot::Mutex::new(DirCacheInner::default()),
            ttl,
            capacity: capacity.max(1),
        }
    }

    /// A cached listing for `path`, if it has not expired.
    pub fn get(&self, path: &Path) -> Option<Vec<DirEntry>> {
        let mut inner = self.inner.lock();
        let (entries, when) = inner.map.get(path)?;
        if when.elapsed() < self.ttl {
            return Some(entries.clone());
        }
        inner.remove_entry(path);
        None
    }

    /// Remember a listing for `path`.
    pub fn put(&self, path: PathBuf, entries: Vec<DirEntry>) {
        let mut inner = self.inner.lock();
        if inner.map.contains_key(&path) {
            inner.map.insert(path, (entries, Instant::now()));
            return;
        }
        while inner.order.len() >= self.capacity {
            if let Some(oldest) = inner.order.pop_front() {
                inner.map.remove(&oldest);
            }
        }
        inner.order.push_back(path.clone());
        inner.map.insert(path, (entries, Instant::now()));
    }

    /// Drop one directory, for a mutation that empties it.
    pub fn invalidate(&self, path: &Path) {
        self.inner.lock().remove_entry(path);
    }

    /// Drop everything at or under `prefix`, for a mutation inside it.
    pub fn invalidate_prefix(&self, prefix: &Path) {
        let mut inner = self.inner.lock();
        // The order list has to agree with the map, or eviction would try to
        // drop entries that are already gone. Collect first so the map is not
        // borrowed across the `order` mutation.
        let dropped: Vec<PathBuf> = inner
            .map
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for path in &dropped {
            inner.map.remove(path);
        }
        inner.order.retain(|k| !dropped.contains(k));
    }

    /// Drop everything, for a change that could touch any path.
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.map.clear();
        inner.order.clear();
    }

    /// How many directories are currently held. Used by the tests.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().map.len()
    }
}

impl DirCacheInner {
    fn remove_entry(&mut self, path: &Path) {
        self.map.remove(path);
        self.order.retain(|k| k != path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adb_proxy::{DirEntry, FileMode};

    fn entry(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            stat: Stat {
                mode: FileMode(FileMode::S_IFREG),
                size: 0,
                mtime: 0,
                atime: 0,
                ctime: 0,
                uid: 0,
                gid: 0,
                nlink: 1,
                blksize: 4096,
                blocks: 0,
            },
        }
    }

    fn listing() -> Vec<DirEntry> {
        vec![entry("a"), entry("b")]
    }

    #[test]
    fn a_fresh_listing_is_returned() {
        let cache = DirCache::new(Duration::from_secs(30), 4);
        cache.put(PathBuf::from("/d"), listing());
        assert_eq!(cache.get(Path::new("/d")).map(|v| v.len()), Some(2));
        assert!(cache.get(Path::new("/other")).is_none());
    }

    #[test]
    fn an_expired_listing_is_dropped() {
        let cache = DirCache::new(Duration::ZERO, 4);
        cache.put(PathBuf::from("/d"), listing());
        assert!(cache.get(Path::new("/d")).is_none());
        assert_eq!(cache.len(), 0, "an expired entry is evicted, not kept");
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_the_oldest() {
        let cache = DirCache::new(Duration::from_secs(30), 3);
        for i in 0..10 {
            cache.put(PathBuf::from(format!("/d{i}")), listing());
        }
        assert_eq!(cache.len(), 3, "capacity is enforced");
        assert!(
            cache.get(Path::new("/d0")).is_none(),
            "the oldest went first"
        );
        assert!(cache.get(Path::new("/d9")).is_some(), "the newest is kept");
    }

    #[test]
    fn re_putting_a_directory_does_not_grow_the_cache() {
        let cache = DirCache::new(Duration::from_secs(30), 3);
        cache.put(PathBuf::from("/d"), listing());
        cache.put(PathBuf::from("/d"), listing());
        cache.put(PathBuf::from("/d"), listing());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(Path::new("/d")).map(|v| v.len()), Some(2));
    }

    #[test]
    fn invalidating_a_prefix_drops_the_whole_subtree() {
        let cache = DirCache::new(Duration::from_secs(30), 8);
        cache.put(PathBuf::from("/a"), listing());
        cache.put(PathBuf::from("/a/b"), listing());
        cache.put(PathBuf::from("/a/b/c"), listing());
        cache.put(PathBuf::from("/z"), listing());

        cache.invalidate_prefix(Path::new("/a"));

        assert!(cache.get(Path::new("/a")).is_none());
        assert!(cache.get(Path::new("/a/b")).is_none());
        assert!(cache.get(Path::new("/a/b/c")).is_none());
        assert!(cache.get(Path::new("/z")).is_some(), "a sibling survives");
        assert_eq!(cache.len(), 1, "the order list is pruned too");
    }

    #[test]
    fn invalidating_one_directory_leaves_the_others() {
        let cache = DirCache::new(Duration::from_secs(30), 8);
        cache.put(PathBuf::from("/a"), listing());
        cache.put(PathBuf::from("/b"), listing());
        cache.invalidate(Path::new("/a"));
        assert!(cache.get(Path::new("/a")).is_none());
        assert!(cache.get(Path::new("/b")).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn clear_empties_everything() {
        let cache = DirCache::new(Duration::from_secs(30), 8);
        cache.put(PathBuf::from("/a"), listing());
        cache.put(PathBuf::from("/b"), listing());
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.get(Path::new("/a")).is_none());
    }
}
