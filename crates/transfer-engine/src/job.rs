//! A single transfer job and its state.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub type JobId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Local (Linux) -> Device.
    Push,
    /// Device -> Local.
    Pull,
}

#[derive(Debug, Clone)]
pub struct JobOptions {
    pub chunk_size: usize,
    pub verify: VerifyMode,
    pub overwrite: OverwriteMode,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            chunk_size: super::DEFAULT_CHUNK,
            verify: VerifyMode::Off,
            overwrite: OverwriteMode::SkipExisting,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerifyMode {
    Off,
    On,
    OnError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverwriteMode {
    Always,
    SkipExisting,
    Resume,
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Pending,
    Running,
    Paused,
    Completed,
    /// The destination already existed and `OverwriteMode::SkipExisting` left
    /// it untouched.
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    pub direction: Direction,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub options: JobOptions,
    /// Optional opaque tag identifying the device this job targets. The
    /// transfer engine itself does not interpret it; dispatchers (e.g. the
    /// daemon) match it against their own device table.
    pub device: Option<String>,
    state: Arc<Mutex<JobState>>,
    bytes_done: Arc<AtomicU64>,
    bytes_total: Arc<AtomicU64>,
    speed_bps: Arc<AtomicU64>,
    eta_secs: Arc<AtomicU64>,
    started: Arc<Mutex<Option<Instant>>>,
    error: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl Job {
    pub fn new(
        id: JobId,
        direction: Direction,
        source: PathBuf,
        destination: PathBuf,
        options: JobOptions,
    ) -> Self {
        Self::with_device(id, direction, source, destination, options, None)
    }

    pub fn with_device(
        id: JobId,
        direction: Direction,
        source: PathBuf,
        destination: PathBuf,
        options: JobOptions,
        device: Option<String>,
    ) -> Self {
        Self {
            id,
            direction,
            source,
            destination,
            options,
            device,
            state: Arc::new(Mutex::new(JobState::Pending)),
            bytes_done: Arc::new(AtomicU64::new(0)),
            bytes_total: Arc::new(AtomicU64::new(0)),
            speed_bps: Arc::new(AtomicU64::new(0)),
            eta_secs: Arc::new(AtomicU64::new(0)),
            started: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn state(&self) -> JobState {
        *self.state.lock()
    }
    pub fn set_state(&self, s: JobState) {
        *self.state.lock() = s;
    }
    pub fn bytes_done(&self) -> u64 {
        self.bytes_done.load(Ordering::Relaxed)
    }
    pub fn bytes_total(&self) -> u64 {
        self.bytes_total.load(Ordering::Relaxed)
    }
    pub fn speed_bps(&self) -> u64 {
        self.speed_bps.load(Ordering::Relaxed)
    }
    pub fn set_speed_bps(&self, s: u64) {
        self.speed_bps.store(s, Ordering::Relaxed);
    }
    pub fn eta_secs(&self) -> u64 {
        self.eta_secs.load(Ordering::Relaxed)
    }
    pub fn set_eta_secs(&self, e: u64) {
        self.eta_secs.store(e, Ordering::Relaxed);
    }
    pub fn add_bytes(&self, n: u64) {
        self.bytes_done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn set_total(&self, n: u64) {
        self.bytes_total.store(n, Ordering::Relaxed);
    }
    pub fn mark_started(&self) {
        *self.started.lock() = Some(Instant::now());
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }
    pub fn set_error(&self, e: impl ToString) {
        *self.error.lock() = Some(e.to_string());
    }

    pub fn reset_for_retry(&self) {
        *self.state.lock() = JobState::Pending;
        self.bytes_done.store(0, Ordering::Relaxed);
        self.bytes_total.store(0, Ordering::Relaxed);
        self.speed_bps.store(0, Ordering::Relaxed);
        self.eta_secs.store(0, Ordering::Relaxed);
        *self.started.lock() = None;
        *self.error.lock() = None;
        self.cancel.store(false, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
    }

    /// Cooperative cancel: the worker checks this between chunks.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Cooperative pause/resume: the worker parks between chunks while set.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        // Only a job that was still going can become paused. A job that already
        // completed, failed or was cancelled keeps the state it reached --
        // relabelling it would make the UI offer Resume for a finished
        // transfer, and resuming it would then rewind the label to Running.
        if matches!(self.state(), JobState::Pending | JobState::Running) {
            self.set_state(JobState::Paused);
        }
    }
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        // Only step back to Running if the job was actually paused. A cancelled
        // or finished job keeps the state it reached, and the worker's own
        // set_state calls win once it drains the park.
        if self.state() == JobState::Paused {
            self.set_state(JobState::Running);
        }
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Fraction of the transfer completed, clamped to [0.0, 1.0].
    ///
    /// When the total size is unknown (or the file is genuinely 0 bytes),
    /// a completed job reports 1.0; anything still in progress reports 0.0.
    pub fn progress_frac(&self) -> f64 {
        let total = self.bytes_total();
        let frac = if total == 0 {
            match self.state() {
                JobState::Completed | JobState::Skipped => 1.0,
                _ => 0.0,
            }
        } else {
            (self.bytes_done() as f64) / (total as f64)
        };
        frac.clamp(0.0, 1.0)
    }
}
