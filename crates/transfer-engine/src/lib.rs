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
//! - Resumption: we record `.adbshare-partial` markers in the destination
//!   directory containing JSON with bytes-completed. On retry, the engine
//!   continues from the last chunk boundary instead of restarting.
//! - Checksums: optional SHA-256 verification at the end of each transfer.

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
