//! Progress reporting: aggregate speed, ETA, history.

use std::collections::VecDeque;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct SpeedSample {
    pub at: Instant,
    pub bytes_done: u64,
    pub bps: f64,
}

#[derive(Debug, Clone)]
pub struct ProgressSnapshot {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current_bps: f64,
    pub avg_bps: f64,
    pub eta: Option<std::time::Duration>,
    pub history: Vec<SpeedSample>,
}

#[derive(Debug)]
pub struct ProgressTracker {
    pub last_sample: Option<(Instant, u64)>,
    pub history: VecDeque<SpeedSample>,
    pub max_history: usize,
    pub started: Instant,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            last_sample: None,
            history: VecDeque::new(),
            max_history: 60,
            started: Instant::now(),
        }
    }

    pub fn tick(&mut self, bytes_done: u64) -> SpeedSample {
        let now = Instant::now();
        let bps = match self.last_sample {
            Some((t, b)) => {
                let dt = now.duration_since(t).as_secs_f64().max(0.001);
                ((bytes_done.saturating_sub(b)) as f64) / dt
            }
            None => 0.0,
        };
        let sample = SpeedSample { at: now, bytes_done, bps };
        self.last_sample = Some((now, bytes_done));
        self.history.push_back(sample);
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
        sample
    }

    pub fn snapshot(&self, total: u64) -> ProgressSnapshot {
        let bytes_done = self.history.back().map(|s| s.bytes_done).unwrap_or(0);
        let current_bps = self.history.back().map(|s| s.bps).unwrap_or(0.0);
        // Average throughput is bytes actually transferred divided by elapsed
        // time — not the total size (which previously inflated avg_bps for
        // partially completed transfers).
        let avg_bps = (bytes_done as f64) / self.started.elapsed().as_secs_f64().max(0.001);
        let eta = if current_bps > 0.0 && total > 0 {
            let remaining = (total.saturating_sub(bytes_done)) as f64;
            Some(std::time::Duration::from_secs_f64(remaining / current_bps))
        } else {
            None
        };
        ProgressSnapshot {
            bytes_done,
            bytes_total: total,
            current_bps,
            avg_bps,
            eta,
            history: self.history.iter().copied().collect(),
        }
    }
}

impl Default for ProgressTracker {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avg_bps_uses_bytes_done_not_total() {
        let mut tracker = ProgressTracker::new();
        // Simulate a 1-second run that has transferred 250 of 1000 bytes.
        tracker.tick(250);
        tracker.started = Instant::now() - std::time::Duration::from_secs(1);

        let snap = tracker.snapshot(1000);
        assert_eq!(snap.bytes_done, 250);
        // elapsed is slightly over 1s, so avg must be slightly under 250.
        assert!(snap.avg_bps < 250.0 && snap.avg_bps > 240.0, "avg_bps = {}", snap.avg_bps);
    }

    #[test]
    fn avg_bps_zero_before_any_bytes() {
        let tracker = ProgressTracker::new();
        let snap = tracker.snapshot(1000);
        assert_eq!(snap.avg_bps, 0.0);
    }
}
