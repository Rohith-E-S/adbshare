//! TTL-based stat cache. Keyed by canonical path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use adb_proxy::Stat;

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
        let inner = self.inner.lock();
        inner.get(path).and_then(|(s, when)| {
            if when.elapsed() < self.ttl { Some(*s) } else { None }
        })
    }

    pub fn put(&self, path: PathBuf, stat: Stat) {
        self.inner.lock().insert(path, (stat, Instant::now()));
    }

    pub fn invalidate(&self, path: &PathBuf) {
        self.inner.lock().remove(path);
    }

    pub fn invalidate_prefix(&self, prefix: &PathBuf) {
        let mut inner = self.inner.lock();
        inner.retain(|k, _| !k.starts_with(prefix));
    }
}
