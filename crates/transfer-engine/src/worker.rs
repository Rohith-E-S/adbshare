use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use adb_proxy::{OpenFlags, ProxyClient, ProxyError};

use super::job::{Direction, Job, JobState, OverwriteMode};
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
        } else if job.state() == JobState::Running {
            // The transfer body may already have set a terminal state
            // (e.g. Skipped for SkipExisting); only mark fresh completions.
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
        let source_len = tokio::fs::metadata(&job.source).await
            .map_err(|e| ProxyError::Other(format!("stat source: {e}")))?
            .len();

        let mut dest = job.destination.to_string_lossy().into_owned();
        let mut offset: u64 = 0;

        // A successful remote stat means the destination exists. A stat
        // error is treated as "does not exist" (only a successful stat
        // triggers SkipExisting/Rename handling).
        let existing = self.client.stat(&dest).await.ok();
        // True when this job is about to create a brand-new file; a
        // pre-existing destination (Always/SkipExisting/Resume) must never
        // be removed on cancel.
        let mut created = existing.is_none();

        let flags = match (&existing, job.options.overwrite) {
            (Some(st), OverwriteMode::SkipExisting) => {
                // The destination already exists: complete without touching it.
                job.set_total(st.size);
                job.add_bytes(st.size);
                job.set_state(JobState::Skipped);
                return Ok(());
            }
            (Some(st), OverwriteMode::Resume) if st.size < source_len => {
                // Continue where the partial destination left off: open
                // WITHOUT TRUNC and start writing at the existing size.
                offset = st.size;
                OpenFlags::CREATE | OpenFlags::WRITE
            }
            (Some(_), OverwriteMode::Rename) => {
                dest = self.pick_remote_rename(&dest).await;
                created = true;
                OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNC
            }
            // Always (and Resume when the destination is already at least as
            // large as the source, which cannot be resumed safely): rewrite
            // from scratch.
            _ => OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNC,
        };

        let mut src = tokio::fs::File::open(&job.source).await.map_err(|e| ProxyError::Other(e.to_string()))?;
        if offset > 0 {
            src.seek(std::io::SeekFrom::Start(offset)).await
                .map_err(|e| ProxyError::Other(format!("seek source: {e}")))?;
            job.add_bytes(offset);
        }
        let dst = self.client.open(&dest, flags, 0o644).await?;

        let mut buf = vec![0u8; self.chunk_size];
        let outcome: Result<(), ProxyError> = loop {
            if job.is_cancelled() || self.wait_while_paused(job).await {
                break Err(ProxyError::Other("cancelled".into()));
            }
            let n = match src.read(&mut buf).await {
                Ok(n) => n,
                Err(e) => break Err(ProxyError::Other(format!("read source: {e}"))),
            };
            if n == 0 { break Ok(()); }
            if let Err(e) = dst.write_at(offset, &buf[..n]).await {
                break Err(e);
            }
            offset += n as u64;
            job.add_bytes(n as u64);
            tracker.tick(offset);
            let snap = tracker.snapshot(job.bytes_total());
            job.set_speed_bps(snap.current_bps as u64);
            job.set_eta_secs(snap.eta.map(|d| d.as_secs()).unwrap_or(0));
        };

        if let Err(e) = outcome {
            let _ = dst.close().await;
            // Don't leave a partial file behind — but only if this job
            // created it; never remove a pre-existing file (Resume keeps
            // its partial destination precisely so it can be resumed).
            if created {
                let _ = self.client.unlink(&dest).await;
            }
            return Err(e);
        }
        // A failed close may mean the device never durably received the
        // final chunks; report it instead of silently completing.
        dst.close().await.map_err(|e| ProxyError::Other(format!("close destination: {e}")))?;
        if matches!(job.options.verify, super::job::VerifyMode::On) {
            self.verify_transfer(job).await?;
        }
        Ok(())
    }

    async fn pull(&self, job: &Job, tracker: &mut ProgressTracker) -> Result<(), ProxyError> {
        // Ensure parent dir exists.
        if let Some(parent) = job.destination.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let mut dest = job.destination.clone();
        let mut offset: u64 = 0;

        // A successful local stat means the destination exists. A stat
        // error is treated as "does not exist".
        let existing = tokio::fs::metadata(&dest).await.ok();
        // True when this job is about to create a brand-new file; a
        // pre-existing destination (Always/SkipExisting/Resume) must never
        // be removed on cancel.
        let mut created = existing.is_none();

        let mut dst: tokio::fs::File = match (&existing, job.options.overwrite) {
            (Some(st), OverwriteMode::SkipExisting) => {
                // The destination already exists: complete without touching it.
                job.set_total(st.len());
                job.add_bytes(st.len());
                job.set_state(JobState::Skipped);
                return Ok(());
            }
            (Some(st), OverwriteMode::Resume) if st.len() < job.bytes_total() => {
                // Continue from the end of the partial local file. The remote
                // read loop below starts at the same offset.
                offset = st.len();
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&dest)
                    .await
                    .map_err(|e| ProxyError::Other(format!("open destination: {e}")))?;
                f.seek(std::io::SeekFrom::Start(offset)).await
                    .map_err(|e| ProxyError::Other(format!("seek destination: {e}")))?;
                job.add_bytes(offset);
                f
            }
            (Some(_), OverwriteMode::Rename) => {
                dest = self.pick_local_rename(&dest).await;
                created = true;
                tokio::fs::File::create(&dest).await
                    .map_err(|e| ProxyError::Other(format!("create destination: {e}")))?
            }
            // Always (and Resume when the local file is already at least as
            // large as the remote source): rewrite from scratch.
            _ => tokio::fs::File::create(&dest).await
                .map_err(|e| ProxyError::Other(format!("create destination: {e}")))?,
        };

        let src = self.client.open(
            job.source.to_str().unwrap(),
            OpenFlags::READ,
            0,
        ).await?;

        let outcome: Result<(), ProxyError> = loop {
            if job.is_cancelled() || self.wait_while_paused(job).await {
                break Err(ProxyError::Other("cancelled".into()));
            }
            let want = self.chunk_size as u32;
            let data = match src.read_at(offset, want).await {
                Ok(d) => d,
                Err(e) => break Err(e),
            };
            if data.is_empty() { break Ok(()); }
            if let Err(e) = dst.write_all(&data).await {
                break Err(ProxyError::Other(format!("write destination: {e}")));
            }
            offset += data.len() as u64;
            job.add_bytes(data.len() as u64);
            tracker.tick(offset);
            let snap = tracker.snapshot(job.bytes_total());
            job.set_speed_bps(snap.current_bps as u64);
            job.set_eta_secs(snap.eta.map(|d| d.as_secs()).unwrap_or(0));
        };

        if let Err(e) = outcome {
            let _ = src.close().await;
            // Don't leave a partial file behind locally — but only if this
            // job created it; never remove a pre-existing file (Resume keeps
            // its partial destination precisely so it can be resumed).
            if created {
                let _ = dst.flush().await;
                drop(dst);
                let _ = tokio::fs::remove_file(&dest).await;
            }
            return Err(e);
        }
        // A failed close may mean the device never registered the final
        // read; report it instead of silently completing.
        src.close().await.map_err(|e| ProxyError::Other(format!("close source: {e}")))?;
        dst.flush().await.map_err(|e| ProxyError::Other(e.to_string()))?;

        if matches!(job.options.verify, super::job::VerifyMode::On) {
            self.verify_transfer(job).await?;
        }
        Ok(())
    }

    /// End-to-end verification: hash the local copy of the file and hash the
    /// remote copy by re-reading it over the proxy protocol, then compare.
    ///
    /// What this guarantees: every byte that arrived on the receiving side
    /// matches the other side as re-read at verification time — it catches
    /// corruption in the transfer path (truncation, lost/mangled chunks,
    /// partially applied writes).
    ///
    /// What this does NOT guarantee: if the device itself serves corrupt
    /// data deterministically (bad storage), both reads agree and the
    /// corruption is not detected. Job options carry no source checksum,
    /// so there is no independent reference hash.
    async fn verify_transfer(&self, job: &Job) -> Result<(), ProxyError> {
        // Local side: push reads/writes source -> remote destination;
        // pull reads/writes remote source -> local destination.
        let (local, remote) = match job.direction {
            Direction::Push => (&job.source, job.destination.to_string_lossy().into_owned()),
            Direction::Pull => (&job.destination, job.source.to_string_lossy().into_owned()),
        };
        let local_hash = verify::sha256_file(local).await
            .map_err(|e| ProxyError::Other(format!("verify: hashing local file failed: {e}")))?;
        let remote_hash = self.hash_remote(&remote).await
            .map_err(|e| ProxyError::Other(format!("verify: hashing remote file failed: {e}")))?;
        if local_hash != remote_hash {
            return Err(ProxyError::Other(format!(
                "verify: checksum mismatch (local sha256 {local_hash}, remote sha256 {remote_hash})"
            )));
        }
        Ok(())
    }

    /// Read a remote file back in chunks and return its SHA-256.
    async fn hash_remote(&self, path: &str) -> Result<String, ProxyError> {
        let f = self.client.open(path, OpenFlags::READ, 0).await?;
        let mut hasher = Sha256::new();
        let mut offset: u64 = 0;
        let want = self.chunk_size.min(u32::MAX as usize) as u32;
        loop {
            let data = f.read_at(offset, want).await?;
            if data.is_empty() { break; }
            hasher.update(&data);
            offset += data.len() as u64;
        }
        let hash = hex::encode(hasher.finalize());
        f.close().await.map_err(|e| ProxyError::Other(format!("close remote file: {e}")))?;
        Ok(hash)
    }

    /// Pick a free "name (1).ext", "name (2).ext", ... path on the device.
    async fn pick_remote_rename(&self, original: &str) -> String {
        let mut cands = RenameCandidates::new(Path::new(original));
        for _ in 0..999 {
            let cand = cands.next();
            if self.client.stat(cand.to_string_lossy().as_ref()).await.is_err() {
                return cand.to_string_lossy().into_owned();
            }
        }
        format!("{original}.adbshare-new")
    }

    /// Pick a free "name (1).ext", "name (2).ext", ... path locally.
    async fn pick_local_rename(&self, original: &Path) -> PathBuf {
        let mut cands = RenameCandidates::new(original);
        for _ in 0..999 {
            let cand = cands.next();
            if tokio::fs::metadata(&cand).await.is_err() {
                return cand;
            }
        }
        original.with_file_name(format!(
            "{}.adbshare-new",
            original.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        ))
    }
}

/// Generates "name (1).ext", "name (2).ext", ... variants of a path.
struct RenameCandidates {
    dir: PathBuf,
    stem: String,
    ext: String,
    n: u32,
}

impl RenameCandidates {
    fn new(path: &Path) -> Self {
        Self {
            dir: path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
            stem: path.file_stem().map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into()),
            ext: path.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default(),
            n: 0,
        }
    }

    fn next(&mut self) -> PathBuf {
        self.n += 1;
        self.dir.join(format!("{} ({}){}", self.stem, self.n, self.ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_candidates_generate_numbered_names() {
        let mut c = RenameCandidates::new(Path::new("/dcim/photo.jpg"));
        assert_eq!(c.next(), PathBuf::from("/dcim/photo (1).jpg"));
        assert_eq!(c.next(), PathBuf::from("/dcim/photo (2).jpg"));
        assert_eq!(c.next(), PathBuf::from("/dcim/photo (3).jpg"));

        let mut c = RenameCandidates::new(Path::new("noext"));
        assert_eq!(c.next(), PathBuf::from("noext (1)"));
    }
}
