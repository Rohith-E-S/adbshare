//! Job queue with parallelism and priority.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::job::{Job, JobId, JobState};

pub struct JobQueue {
    pending: Mutex<VecDeque<Job>>,
    in_flight: Mutex<Vec<Job>>,
    completed: Mutex<Vec<Job>>,
    next_id: AtomicU64,
    pub parallelism: usize,
    notify: mpsc::UnboundedSender<()>,
    pub on_update: Option<Arc<dyn Fn(&Job) + Send + Sync>>,
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
        (Arc::new(Self {
            pending: Mutex::new(VecDeque::new()),
            in_flight: Mutex::new(Vec::new()),
            completed: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            parallelism,
            notify: tx,
            on_update: None,
        }), rx)
    }

    pub fn submit(&self, mut job: Job) -> JobId {
        if job.id == 0 {
            job.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        }
        self.pending.lock().push_back(job.clone());
        let _ = self.notify.send(());
        if let Some(cb) = &self.on_update { cb(&job); }
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
        self.completed.lock().push(job);
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
    pub fn jobs_snapshot(&self) -> Vec<Job> {
        let mut out = Vec::new();
        out.extend(self.pending.lock().iter().cloned());
        out.extend(self.in_flight.lock().iter().cloned());
        out.extend(self.completed.lock().iter().cloned());
        out
    }

    /// Pop the next pending job if the queue has spare capacity. The job
    /// is moved from `pending` into `in_flight` so it remains visible to
    /// `jobs_snapshot` while running. Callers must invoke `mark_done` when
    /// the job finishes.
    pub fn try_dispatch(&self) -> Option<Job> {
        if !self.has_capacity() { return None; }
        let job = self.next_pending()?;
        self.in_flight.lock().push(job.clone());
        Some(job)
    }

    /// Look up a job by id anywhere in the queue. Job handles share their
    /// state/flags via Arc, so mutating the clone affects the live job.
    pub fn find(&self, id: JobId) -> Option<Job> {
        {
            let q = self.pending.lock();
            if let Some(j) = q.iter().find(|j| j.id == id) { return Some(j.clone()); }
        }
        {
            let f = self.in_flight.lock();
            if let Some(j) = f.iter().find(|j| j.id == id) { return Some(j.clone()); }
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
                let mut job = q.remove(pos).unwrap();
                job.cancel();
                job.set_error("cancelled");
                job.set_state(JobState::Cancelled);
                self.completed.lock().push(job);
                return true;
            }
        }
        self.find(id).map(|j| j.cancel()).is_some()
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
}
