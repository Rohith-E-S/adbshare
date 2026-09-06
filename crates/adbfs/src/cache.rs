//! TTL-based stat cache. Keyed by canonical path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use adb_proxy::Stat;

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
        Self { inner: parking_lot::Mutex::new(HashMap::new()), ttl }
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
