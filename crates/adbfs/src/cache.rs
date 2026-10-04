//! TTL-based caches for the FUSE layer: stats, keyed by canonical path, and
//! directory listings, keyed the same way.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adb_proxy::{DirEntry, Stat};

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

    pub fn get(&self, path: &Path) -> Option<Stat> {
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

    pub fn invalidate(&self, path: &Path) {
        self.inner.lock().remove(path);
    }

    pub fn invalidate_prefix(&self, prefix: &Path) {
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
///
/// A listing is handed out as a shared `Arc` rather than a copy: it is a `Vec`
/// of a name and a `Stat` per entry, readdir iterates all of it, and copying
/// it while the cache is locked would make every cache hit pay for the whole
/// directory.
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
    map: HashMap<PathBuf, (Arc<Vec<DirEntry>>, Instant)>,
    /// Eviction order, oldest first. A re-put moves a directory to the back; a
    /// `get` does not, so this is insertion order rather than least-recently-used.
    order: VecDeque<PathBuf>,
    /// Bumped by every invalidation. A listing fetched before a bump describes
    /// the directory as it was, so storing it afterwards would resurrect the
    /// change the invalidation was for.
    epoch: u64,
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
    pub fn get(&self, path: &Path) -> Option<Arc<Vec<DirEntry>>> {
        let mut inner = self.inner.lock();
        let (entries, when) = inner.map.get(path)?;
        if when.elapsed() < self.ttl {
            return Some(Arc::clone(entries));
        }
        inner.remove_entry(path);
        None
    }

    /// The current invalidation epoch, for [`Self::put`] to check against.
    pub fn epoch(&self) -> u64 {
        self.inner.lock().epoch
    }

    /// Remember a listing for `path`, unless it was fetched before `epoch`.
    pub fn put(&self, path: PathBuf, entries: Arc<Vec<DirEntry>>, epoch: u64) {
        let mut inner = self.inner.lock();
        if inner.epoch != epoch {
            return;
        }
        // A re-put is a use, so the directory moves to the back of the
        // eviction order. Leaving it where it was would evict the directory a
        // walk keeps coming back to before one it has just passed through.
        if let Some(pos) = inner.order.iter().position(|k| *k == path) {
            inner.order.remove(pos);
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
        let mut inner = self.inner.lock();
        inner.epoch = inner.epoch.wrapping_add(1);
        inner.remove_entry(path);
    }

    /// Drop everything at or under `prefix`, for a mutation inside it.
    pub fn invalidate_prefix(&self, prefix: &Path) {
        let mut inner = self.inner.lock();
        inner.epoch = inner.epoch.wrapping_add(1);
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
}

impl DirCacheInner {
    fn remove_entry(&mut self, path: &Path) {
        self.map.remove(path);
        self.order.retain(|k| k != path);
    }
}
