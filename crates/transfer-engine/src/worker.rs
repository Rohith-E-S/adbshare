use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use adb_proxy::{OpenFlags, ProxyClient, ProxyError};

use super::job::{Direction, Job, JobState};
use super::progress::ProgressTracker;
use super::verify;
use super::DEFAULT_CHUNK;

#[derive(Debug)]
pub struct Worker {
    pub client: ProxyClient,
    pub chunk_size: usize,
}

impl Worker {
    pub fn new(client: ProxyClient) -> Self {
        Self { client, chunk_size: DEFAULT_CHUNK }
    }

    pub fn with_chunk(mut self, n: usize) -> Self { self.chunk_size = n; self }

    /// Run a job to completion. Updates job state and progress.
    pub async fn run(self: Arc<Self>, job: Job) -> Result<(), ProxyError> {
        if job.is_cancelled() {
            job.set_error("cancelled");
            job.set_state(JobState::Cancelled);
            return Ok(());
        }
        job.set_state(JobState::Running);
        job.mark_started();
        let mut tracker = ProgressTracker::new();

        // Determine total size for progress reporting.
        let total = match job.direction {
            Direction::Push => {
                let meta = tokio::fs::metadata(&job.source).await.map_err(|e| ProxyError::Other(e.to_string()))?;
                meta.len()
            }
            Direction::Pull => {
                self.client.stat(job.source.to_str().unwrap()).await.map(|s| s.size).unwrap_or(0)
            }
        };
        job.set_total(total);

        let result = match job.direction {
            Direction::Push => self.push(&job, &mut tracker).await,
            Direction::Pull => self.pull(&job, &mut tracker).await,
        };

        if job.is_cancelled() {
            job.set_error("cancelled");
            job.set_state(JobState::Cancelled);
            return Ok(());
        }
        if let Err(e) = &result {
            job.set_error(e.to_string());
            job.set_state(JobState::Failed);
        } else {
            job.set_state(JobState::Completed);
        }
        result
    }

    /// Park while paused; returns true if the job was cancelled while parked.
    async fn wait_while_paused(&self, job: &Job) -> bool {
        while job.is_paused() && !job.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        job.is_cancelled()
    }

    async fn push(&self, job: &Job, tracker: &mut ProgressTracker) -> Result<(), ProxyError> {
        let mut src = tokio::fs::File::open(&job.source).await.map_err(|e| ProxyError::Other(e.to_string()))?;
        let dst = self.client.open(
            job.destination.to_str().unwrap(),
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNC,
            0o644,
        ).await?;

        let mut offset: u64 = 0;
        let mut buf = vec![0u8; self.chunk_size];
        loop {
            if job.is_cancelled() || self.wait_while_paused(job).await {
                let _ = dst.close().await;
                // Don't leave a partial file behind on the device.
                let _ = self.client.unlink(job.destination.to_str().unwrap_or("")).await;
                return Err(ProxyError::Other("cancelled".into()));
            }
            let n = src.read(&mut buf).await.map_err(|e| ProxyError::Other(e.to_string()))?;
            if n == 0 { break; }
            dst.write_at(offset, &buf[..n]).await?;
            offset += n as u64;
            job.add_bytes(n as u64);
            tracker.tick(offset);
            let snap = tracker.snapshot(job.bytes_total());
            job.set_speed_bps(snap.current_bps as u64);
            job.set_eta_secs(snap.eta.map(|d| d.as_secs()).unwrap_or(0));
        }
        let _ = dst.close().await;
        Ok(())
    }

    async fn pull(&self, job: &Job, tracker: &mut ProgressTracker) -> Result<(), ProxyError> {
        // Ensure parent dir exists.
        if let Some(parent) = job.destination.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let src = self.client.open(
            job.source.to_str().unwrap(),
            OpenFlags::READ,
            0,
        ).await?;
        let mut dst = tokio::fs::File::create(&job.destination).await.map_err(|e| ProxyError::Other(e.to_string()))?;

        let total = job.bytes_total();
        let mut offset: u64 = 0;
        while offset < total || total == 0 {
            if job.is_cancelled() || self.wait_while_paused(job).await {
                let _ = src.close().await;
                // Don't leave a partial file behind locally.
                let _ = dst.flush().await;
                drop(dst);
                let _ = tokio::fs::remove_file(&job.destination).await;
                return Err(ProxyError::Other("cancelled".into()));
            }
            let want = self.chunk_size as u32;
            let data = src.read_at(offset, want).await?;
            if data.is_empty() { break; }
            dst.write_all(&data).await.map_err(|e| ProxyError::Other(e.to_string()))?;
            offset += data.len() as u64;
            job.add_bytes(data.len() as u64);
            tracker.tick(offset);
            let snap = tracker.snapshot(job.bytes_total());
            job.set_speed_bps(snap.current_bps as u64);
            job.set_eta_secs(snap.eta.map(|d| d.as_secs()).unwrap_or(0));
        }
        let _ = src.close().await;
        dst.flush().await.map_err(|e| ProxyError::Other(e.to_string()))?;

        if matches!(job.options.verify, super::job::VerifyMode::On) {
            let _ = verify::verify_checksum(&job.destination, "").await;
        }
        Ok(())
    }
}
