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

    pub fn next_pending(&self) -> Option<Job> {
        let mut q = self.pending.lock();
        q.pop_front()
    }

    pub fn mark_running(&self, job: Job) {
        self.in_flight.lock().push(job);
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

    pub fn has_capacity(&self) -> bool {
        self.in_flight.lock().len() < self.parallelism
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            pending: self.pending.lock().len(),
            in_flight: self.in_flight.lock().len(),
            completed: self.completed.lock().len(),
        }
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

    /// Requeue every `Failed` job whose source/destination paths (or device tag)
    /// contain `serial` as `Pending` so the dispatcher retries it after a
    /// replug. Returns the number of jobs requeued.
    pub fn retry_failed_for(&self, serial: &str) -> u64 {
        if serial.is_empty() {
            return 0;
        }
        let mut pending = self.pending.lock();
        let mut completed = self.completed.lock();
        let mut count: u64 = 0;
        completed.retain(|job| {
            let mine = job.device.as_deref().is_some_and(|d| d.contains(serial))
                || job.source.to_string_lossy().contains(serial)
                || job.destination.to_string_lossy().contains(serial);
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

#[derive(Debug, Clone, Copy)]
pub struct QueueSnapshot {
    pub pending: usize,
    pub in_flight: usize,
    pub completed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Direction, JobOptions};

    fn test_job(name: &str) -> Job {
        Job::new(
            0,
            Direction::Push,
            std::path::PathBuf::from(format!("/src/{name}")),
            std::path::PathBuf::from(format!("/dst/{name}")),
            JobOptions::default(),
        )
    }

    #[test]
    fn retry_job_resets_progress_error_and_flags() {
        let (queue, _rx) = JobQueue::new(1);
        let id = queue.submit(test_job("retry"));
        let job = queue.try_dispatch().expect("dispatches");
        assert_eq!(job.id, id);
        job.add_bytes(50);
        job.set_total(100);
        job.set_error("boom");
        job.pause();
        job.cancel();
        job.set_state(JobState::Failed);
        queue.mark_done(job);
        assert!(queue.retry_job(id));
        let job = queue.try_dispatch().expect("redispatched");
        assert_eq!(job.id, id);
        assert_eq!(job.state(), JobState::Pending);
        assert_eq!(job.bytes_done(), 0);
        assert_eq!(job.bytes_total(), 0);
        assert!(job.error().is_none());
        assert!(!job.is_paused());
        assert!(!job.is_cancelled());
        assert!(!queue.retry_job(id), "only terminal completed jobs retry");
    }

    #[tokio::test]
    async fn mark_done_wakes_dispatcher() {
        let (queue, mut rx) = JobQueue::new(1);
        queue.submit(test_job("a"));
        queue.submit(test_job("b"));
        // The submit notifications arrive first.
        rx.try_recv().expect("submit notifies");

        let a = queue.try_dispatch().expect("first job dispatches");
        let a_id = a.id;
        // Parallelism is saturated: nothing else may dispatch.
        assert!(queue.try_dispatch().is_none());
        assert_eq!(queue.snapshot().in_flight, 1);

        queue.mark_done(a);
        // mark_done must wake the (otherwise sleeping) dispatcher loop and
        // the freed slot must let the pending job start.
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("mark_done must notify the dispatcher")
            .expect("channel open");
        let b = queue.try_dispatch().expect("freed slot allows dispatch");
        assert_ne!(b.id, a_id);
    }

    #[test]
    fn try_dispatch_never_exceeds_parallelism() {
        let (queue, _rx) = JobQueue::new(2);
        for i in 0..32 {
            queue.submit(test_job(&format!("j{i}")));
        }
        let queue = queue.clone();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let q = Arc::clone(&queue);
                std::thread::spawn(move || {
                    let mut got = 0;
                    while q.try_dispatch().is_some() {
                        got += 1;
                    }
                    got
                })
            })
            .collect();
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 2, "exactly `parallelism` jobs may be in flight");
        assert_eq!(queue.snapshot().in_flight, 2);
        assert_eq!(queue.snapshot().pending, 30);
    }

    #[tokio::test]
    async fn jobs_snapshot_never_duplicates_or_drops_a_job() {
        let (queue, _rx) = JobQueue::new(2);
        for i in 0..64 {
            queue.submit(test_job(&format!("j{i}")));
        }

        let q = Arc::clone(&queue);
        let churn = std::thread::spawn(move || {
            for _ in 0..500 {
                while let Some(job) = q.try_dispatch() {
                    q.mark_done(job);
                }
            }
        });

        let mut seen_dupe = false;
        for _ in 0..2000 {
            let snap = queue.jobs_snapshot();
            let mut ids: Vec<_> = snap.iter().map(|j| j.id).collect();
            let n = ids.len();
            ids.sort_unstable();
            ids.dedup();
            if ids.len() != n {
                seen_dupe = true;
                break;
            }
        }
        churn.join().unwrap();
        assert!(!seen_dupe, "a job appeared more than once in a snapshot");
        // After the churn completes, every job is accounted for exactly once.
        assert_eq!(queue.jobs_snapshot().len(), 64);
    }
}

#[cfg(test)]
mod completed_bound_tests {
    use super::super::Direction;
    use super::super::JobOptions;
    use super::*;

    fn job(i: u64) -> Job {
        Job::with_device(
            i,
            Direction::Push,
            std::path::PathBuf::from(format!("/src/{i}")),
            std::path::PathBuf::from(format!("/dst/{i}")),
            JobOptions::default(),
            None,
        )
    }

    #[test]
    fn the_completed_list_is_bounded() {
        let (queue, _rx) = JobQueue::new(1);
        for i in 0..(MAX_COMPLETED as u64 * 3) {
            queue.submit(job(i));
            let taken = queue.try_dispatch().expect("a slot is free");
            queue.mark_done(taken);
        }
        assert_eq!(
            queue.snapshot().completed,
            MAX_COMPLETED,
            "the history must not grow without bound"
        );
    }

    #[test]
    fn trimming_keeps_the_newest_jobs() {
        let (queue, _rx) = JobQueue::new(1);
        let total = MAX_COMPLETED as u64 + 10;
        for i in 0..total {
            queue.submit(job(i));
            let taken = queue.try_dispatch().expect("a slot is free");
            queue.mark_done(taken);
        }
        let snapshot = queue.jobs_snapshot();
        assert_eq!(snapshot.len(), MAX_COMPLETED);
        assert_eq!(
            snapshot.first().map(|j| j.id),
            Some(10),
            "the oldest ten were dropped"
        );
        assert_eq!(snapshot.last().map(|j| j.id), Some(total - 1));
    }

    #[test]
    fn a_full_history_does_not_block_new_work() {
        let (queue, _rx) = JobQueue::new(1);
        for i in 0..(MAX_COMPLETED as u64 + 50) {
            queue.submit(job(i));
            let taken = queue.try_dispatch().expect("a slot is free");
            queue.mark_done(taken);
        }
        queue.submit(job(9_999));
        assert!(
            queue.try_dispatch().is_some(),
            "a trimmed history must not stop dispatching"
        );
    }

    #[test]
    fn cancelling_a_queueful_of_transfers_keeps_the_history_bounded() {
        let (queue, _rx) = JobQueue::new(1);
        // Ids start at 1, or `submit` would assign one of its own.
        let total = MAX_COMPLETED as u64 + 10;
        for i in 1..=total {
            queue.submit(job(i));
            assert!(queue.cancel_job(i), "a queued job cancels straight away");
        }
        assert_eq!(queue.snapshot().pending, 0);
        assert_eq!(
            queue.snapshot().completed,
            MAX_COMPLETED,
            "cancelling must not grow the history without bound"
        );
        let snapshot = queue.jobs_snapshot();
        assert_eq!(
            snapshot.first().map(|j| j.id),
            Some(11),
            "the oldest ten went"
        );
        assert_eq!(snapshot.last().map(|j| j.id), Some(total));
    }
}

/// What the daemon's `list_jobs` does, twice a second, for the GUI's poll.
#[cfg(test)]
mod poll_cost_tests {
    use super::*;
    use crate::job::{Direction, JobOptions, OverwriteMode, VerifyMode};

    /// Fill the queue with `jobs` completed transfers, as a long session would.
    fn filled(jobs: u64) -> (Arc<JobQueue>, mpsc::UnboundedReceiver<()>) {
        let (queue, rx) = JobQueue::new(1);
        for i in 0..jobs {
            let job = Job::new(
                i + 1,
                Direction::Push,
                std::path::PathBuf::from(format!("/very/long/source/path/{i}.bin")),
                std::path::PathBuf::from(format!("/a/destination/path/{i}.bin")),
                JobOptions {
                    overwrite: OverwriteMode::SkipExisting,
                    verify: VerifyMode::On,
                    chunk_size: super::super::DEFAULT_CHUNK,
                },
            );
            queue.submit(job);
            let taken = queue.try_dispatch().expect("a slot is free");
            queue.mark_done(taken);
        }
        (queue, rx)
    }

    #[test]
    fn the_poll_cost_is_flat_once_the_history_is_full() {
        // `list_jobs` clones the snapshot and encodes it to JSON on every GUI
        // poll. Before the cap this grew without limit; now the cost at 10x the
        // history is the same as at the cap.
        //
        // The size assertions are the whole test. A wall-clock comparison would
        // prove nothing here: both queues are trimmed to exactly
        // MAX_COMPLETED, so `encode` does identical work and any ratio between
        // them is timing noise.
        let (small, _a) = filled(MAX_COMPLETED as u64);
        let (large, _b) = filled(MAX_COMPLETED as u64 * 10);
        assert_eq!(small.snapshot().completed, MAX_COMPLETED);
        assert_eq!(
            large.snapshot().completed,
            MAX_COMPLETED,
            "a ten-times-longer session keeps the same history"
        );
    }
}
