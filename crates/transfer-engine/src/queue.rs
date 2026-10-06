//! Job queue with parallelism and priority.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::job::{Job, JobId, JobState};

/// How many finished jobs to remember.
///
/// The completed list was previously unbounded, and it is not just memory: the
/// GUI polls `list_jobs` twice a second, and every poll clones and JSON-encodes
/// the whole list. A long session would grow that without limit and make each
/// poll slower forever. A few hundred entries is far more history than the
/// transfers popover shows, and it bounds both the memory and the poll.
///
/// The cost is that an individual job that finished more than this long ago
/// can no longer be retried by id; `retry_failed` only looks at the retained
/// window. Retrying is offered from the UI, not as a durable operation.
pub const MAX_COMPLETED: usize = 200;

/// Trim the finished-job history back to `MAX_COMPLETED`.
///
/// Oldest first, so the front is what falls off. Every path that puts a job
/// into the history goes through here; `cancel_job` once did not, so cancelling
/// a queueful of transfers undid the bound for the GUI's poll.
fn trim_completed(completed: &mut Vec<Job>) {
    if completed.len() > MAX_COMPLETED {
        let excess = completed.len() - MAX_COMPLETED;
        completed.drain(0..excess);
    }
}

/// Called after a job's progress or state changes.
pub type OnUpdate = Arc<dyn Fn(&Job) + Send + Sync>;

pub struct JobQueue {
    pending: Mutex<VecDeque<Job>>,
    in_flight: Mutex<Vec<Job>>,
    completed: Mutex<Vec<Job>>,
    next_id: AtomicU64,
    pub parallelism: usize,
    notify: mpsc::UnboundedSender<()>,
    pub on_update: Option<OnUpdate>,
}

impl std::fmt::Debug for JobQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobQueue")
            .field("parallelism", &self.parallelism)
            .field("pending", &self.pending.lock().len())
            .field("in_flight", &self.in_flight.lock().len())
            .field("completed", &self.completed.lock().len())
            .finish()
    }
}

impl JobQueue {
    pub fn new(parallelism: usize) -> (Arc<Self>, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                pending: Mutex::new(VecDeque::new()),
                in_flight: Mutex::new(Vec::new()),
                completed: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                parallelism,
                notify: tx,
                on_update: None,
            }),
            rx,
        )
    }

    pub fn submit(&self, mut job: Job) -> JobId {
        if job.id == 0 {
            job.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        }
        self.pending.lock().push_back(job.clone());
        let _ = self.notify.send(());
        if let Some(cb) = &self.on_update {
            cb(&job);
        }
        job.id
    }

    pub fn mark_done(&self, job: Job) {
        let mut inflight = self.in_flight.lock();
        inflight.retain(|j| j.id != job.id);
        drop(inflight);
        let mut completed = self.completed.lock();
        completed.push(job);
        trim_completed(&mut completed);
        drop(completed);
        // A parallelism slot just freed up; wake the dispatcher so a waiting
        // `try_dispatch` loop re-checks capacity and starts pending jobs.
        // Best-effort: a closed receiver simply means nobody is dispatching.
        let _ = self.notify.send(());
    }

    /// Take a snapshot of every job currently in the queue (pending, in-flight,
    /// completed). The returned `Vec` is a copy of the current state; the
    /// caller may iterate without holding any lock.
    ///
    /// All three guards are held simultaneously (in the queue's global lock
    /// order `pending` -> `in_flight` -> `completed`) so a job that moves
    /// between lists during the snapshot can neither appear twice nor
    /// transiently vanish. Every other locking site on this struct follows
    /// the same order, so simultaneous acquisition cannot deadlock.
    pub fn jobs_snapshot(&self) -> Vec<Job> {
        let pending = self.pending.lock();
        let inflight = self.in_flight.lock();
        let completed = self.completed.lock();
        let mut out = Vec::with_capacity(pending.len() + inflight.len() + completed.len());
        out.extend(pending.iter().cloned());
        out.extend(inflight.iter().cloned());
        out.extend(completed.iter().cloned());
        out
    }

    /// Pop the next pending job if the queue has spare capacity. The job
    /// is moved from `pending` into `in_flight` so it remains visible to
    /// `jobs_snapshot` while running. Callers must invoke `mark_done` when
    /// the job finishes.
    ///
    /// The capacity check and the move into `in_flight` happen atomically
    /// under the `in_flight` lock, so concurrent dispatchers can never
    /// exceed `parallelism`. Guards are acquired in the queue's global lock
    /// order (`pending` -> `in_flight` -> `completed`, see `jobs_snapshot`).
    pub fn try_dispatch(&self) -> Option<Job> {
        let mut q = self.pending.lock();
        let mut inflight = self.in_flight.lock();
        if inflight.len() >= self.parallelism {
            return None;
        }
        let job = q.pop_front()?;
        inflight.push(job.clone());
        Some(job)
    }

    /// Look up a job by id anywhere in the queue. Job handles share their
    /// state/flags via Arc, so mutating the clone affects the live job.
    pub fn find(&self, id: JobId) -> Option<Job> {
        {
            let q = self.pending.lock();
            if let Some(j) = q.iter().find(|j| j.id == id) {
                return Some(j.clone());
            }
        }
        {
            let f = self.in_flight.lock();
            if let Some(j) = f.iter().find(|j| j.id == id) {
                return Some(j.clone());
            }
        }
        self.completed.lock().iter().find(|j| j.id == id).cloned()
    }

    /// Pause a job. Pending jobs stay queued; running jobs park between
    /// chunks until resumed.
    pub fn pause_job(&self, id: JobId) -> bool {
        self.find(id).map(|j| j.pause()).is_some()
    }

    pub fn resume_job(&self, id: JobId) -> bool {
        self.find(id).map(|j| j.resume()).is_some()
    }

    /// Cancel a job. Pending jobs are removed from the queue immediately;
    /// running jobs stop at the next chunk boundary.
    pub fn cancel_job(&self, id: JobId) -> bool {
        {
            let mut q = self.pending.lock();
            if let Some(pos) = q.iter().position(|j| j.id == id) {
                let job = q.remove(pos).unwrap();
                job.cancel();
                job.set_error("cancelled");
                job.set_state(JobState::Cancelled);
                let mut completed = self.completed.lock();
                completed.push(job);
                trim_completed(&mut completed);
                return true;
            }
        }
        self.find(id).map(|j| j.cancel()).is_some()
    }

    pub fn retry_job(&self, id: JobId) -> bool {
        let mut completed = self.completed.lock();
        let pos = completed.iter().position(|j| {
            j.id == id
                && matches!(
                    j.state(),
                    JobState::Failed | JobState::Cancelled | JobState::Skipped
                )
        });
        let Some(job) = pos.map(|p| completed.remove(p)) else {
            return false;
        };
        drop(completed);
        job.reset_for_retry();
        self.pending.lock().push_back(job);
        let _ = self.notify.send(());
        true
    }

    /// Requeue every `Failed` job as `Pending` so the dispatcher retries it.
    /// Returns the number of jobs requeued.
    pub fn retry_failed(&self) -> u64 {
        let mut pending = self.pending.lock();
        let mut completed = self.completed.lock();
        let mut count: u64 = 0;
        completed.retain(|job| {
            if job.state() == JobState::Failed {
                job.reset_for_retry();
                pending.push_back(job.clone());
                count += 1;
                false
            } else {
                true
            }
        });
        drop(pending);
        drop(completed);
        if count > 0 {
            let _ = self.notify.send(());
        }
        count
    }

    /// Requeue every `Failed` job whose device tag is exactly `serial` as
    /// `Pending`, so the dispatcher retries it after a replug. Returns the
    /// number of jobs requeued.
    ///
    /// Matching is on the device tag alone and is exact. Neither the paths nor
    /// a substring test take part: those cross-matched, so replugging one phone
    /// resent another phone's transfers.
    pub fn retry_failed_for(&self, serial: &str) -> u64 {
        if serial.is_empty() {
            return 0;
        }
        let mut pending = self.pending.lock();
        let mut completed = self.completed.lock();
        let mut count: u64 = 0;
        completed.retain(|job| {
            // Exact match on the device tag only. This used to be a substring
            // test over the tag *and* both paths, which cross-matched three
            // ways: serials that are prefixes of one another, a tag that
            // merely contains the serial, and — worst — the local source or
            // destination, so a job for `/home/me/ABC123-report.pdf` was
            // requeued when phone `ABC123` was plugged back in. Replugging one
            // phone would then resend another phone's transfers.
            let mine = job.device.as_deref() == Some(serial);
            if mine && job.state() == JobState::Failed {
                job.reset_for_retry();
                pending.push_back(job.clone());
                count += 1;
                false
            } else {
                true
            }
        });
        drop(pending);
        drop(completed);
        if count > 0 {
            let _ = self.notify.send(());
        }
        count
    }
}
