//! Transfer engine: queue, parallelism, resumption, checksums.
//!
//! The engine is the entry point for one-off operations that bypass FUSE
//! (e.g. the user clicked "download all photos"). It also powers the daemon's
//! background sync workers.
//!
//! ## Design
//!
//! - `Job` is a single file transfer: source, destination, direction.
//! - `JobQueue` holds pending/in-flight jobs and dispatches up to
//!   `parallelism` of them concurrently to a `Worker`.
//! - `Worker` uses `adb_proxy::ProxyFile` for random-access I/O.
//! - Overwrite modes: with `OverwriteMode::SkipExisting` (the default) a
//!   transfer to an existing destination completes as `JobState::Skipped`
//!   without touching the file. `Resume` continues in place from the
//!   destination's current size (detected via stat — there are no
//!   `.adbshare-partial` marker files). `Rename` writes to a free
//!   `name (N).ext` alternate. Partial files are only cleaned up when the
//!   job itself created the destination.
//! - Checksums: `VerifyMode::On` re-reads and hashes both sides (SHA-256)
//!   after the transfer and fails the job on mismatch.

#![warn(missing_debug_implementations)]

pub mod job;
pub mod progress;
pub mod queue;
pub mod verify;
pub mod worker;

pub use job::{Direction, Job, JobId, JobOptions, JobState};
pub use progress::{ProgressSnapshot, SpeedSample};
pub use queue::JobQueue;
pub use verify::verify_checksum;
pub use worker::Worker;

pub const DEFAULT_CHUNK: usize = 2 * 1024 * 1024; // 2 MiB
pub const DEFAULT_PARALLELISM: usize = 4;
