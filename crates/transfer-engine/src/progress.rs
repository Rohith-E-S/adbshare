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
        let current_bps = self.history.back().map(|s| s.bps).unwrap_or(0.0);
        let avg_bps = if total == 0 {
            0.0
        } else {
            (total as f64) / self.started.elapsed().as_secs_f64().max(0.001)
        };
        let eta = if current_bps > 0.0 && total > 0 {
            let remaining = (total.saturating_sub(self.history.back().map(|s| s.bytes_done).unwrap_or(0))) as f64;
            Some(std::time::Duration::from_secs_f64(remaining / current_bps))
        } else {
            None
        };
        ProgressSnapshot {
            bytes_done: self.history.back().map(|s| s.bytes_done).unwrap_or(0),
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
