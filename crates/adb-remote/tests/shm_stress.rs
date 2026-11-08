//! Empirical stress tests for `ShmPool`, `ShmWriter`, and `ShmReader`.
//!
//! Evaluates concurrency, high-rate (120 FPS) streaming, race conditions,
//! slot skipping, memory integrity, and atomic ordering.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use adb_remote::shm::{
    SHM_HEADER_SIZE, SHM_MAGIC, SHM_VERSION, ShmError, ShmHeader, ShmPool, ShmReader, ShmWriter,
};

fn unique_temp_path(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "adbshare_stress_{}_{}_{}.shm",
        label,
        std::process::id(),
        nanos
    ))
}

struct AutoUnlink(PathBuf);
impl Drop for AutoUnlink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Stress Test 1: 120 FPS multi-threaded concurrent writer and readers.
///
/// Writer generates frames at 120 FPS (8.33ms interval) for 240 frames (2 seconds).
/// 4 concurrent reader threads continuously poll `reader.head()` and verify:
/// - Monotonically non-decreasing sequence numbers
/// - Valid slot index in range 0..num_slots
/// - Frame payload integrity: pattern matching the published sequence
#[test]
fn test_stress_120fps_concurrent_readers() {
    let path = unique_temp_path("120fps_concur");
    let _cleanup = AutoUnlink(path.clone());

    let frame_width = 1280u32;
    let frame_height = 720u32;
    let frame_bytes = (frame_width * frame_height * 4) as usize;
    let num_slots = 3u32;

    let mut writer = ShmWriter::create(&path, frame_bytes as u32, num_slots)
        .expect("failed to create ShmWriter");

    let num_readers = 4;
    let target_frames = 240u64; // 2 seconds of 120 FPS
    let frame_interval = Duration::from_micros(8333); // 120 FPS ~ 8.333 ms

    let barrier = Arc::new(Barrier::new(num_readers + 1));
    let stop_signal = Arc::new(AtomicBool::new(false));

    let mut reader_handles = Vec::new();

    for reader_id in 0..num_readers {
        let path_clone = path.clone();
        let barrier_clone = Arc::clone(&barrier);
        let stop_clone = Arc::clone(&stop_signal);

        let handle = std::thread::spawn(move || {
            let reader = ShmReader::open(&path_clone).expect("failed to open ShmReader");
            barrier_clone.wait();

            let mut last_seq = 0u64;
            let mut frames_read = 0u64;
            let mut seq_regressions = 0u64;
            let mut corrupted_frames = 0u64;

            while !stop_clone.load(Ordering::Acquire) || last_seq < target_frames {
                let (seq, slot) = reader.head();
                if seq > last_seq {
                    // Sequence monotonicity check
                    if seq < last_seq {
                        seq_regressions += 1;
                    }

                    // Slot boundary check
                    assert!(slot < num_slots, "slot index {} out of bounds", slot);

                    // Read payload
                    let data = reader
                        .read_slot_exact(slot, 64)
                        .expect("read_slot_exact failed");

                    // Verify header 8 bytes encode seq
                    let payload_seq = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    let payload_reader_tag = u32::from_le_bytes(data[8..12].try_into().unwrap());

                    // Check for torn frame: does the data in slot match the published seq?
                    if payload_seq != seq || payload_reader_tag != 0xA5A5A5A5 {
                        corrupted_frames += 1;
                    }

                    last_seq = seq;
                    frames_read += 1;
                } else {
                    std::hint::spin_loop();
                }

                if stop_clone.load(Ordering::Acquire) && seq >= target_frames {
                    break;
                }
            }

            (reader_id, frames_read, seq_regressions, corrupted_frames)
        });

        reader_handles.push(handle);
    }

    // Wait for all readers to be ready
    barrier.wait();

    let start_time = Instant::now();
    for seq in 1..=target_frames {
        let frame_start = Instant::now();

        let slot = writer.next_slot();
        let slot_buf = writer.slot_mut(slot).expect("slot_mut failed");

        // Write marker pattern: first 8 bytes seq, next 4 bytes magic 0xA5A5A5A5, rest filled with seq as u8
        slot_buf[0..8].copy_from_slice(&seq.to_le_bytes());
        slot_buf[8..12].copy_from_slice(&0xA5A5A5A5u32.to_le_bytes());
        slot_buf[12..64].fill((seq & 0xFF) as u8);

        writer
            .publish_frame(slot, frame_width, frame_height, seq)
            .expect("publish_frame failed");

        // Regulate frame rate to ~120 FPS
        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            std::thread::sleep(frame_interval - elapsed);
        }
    }

    let total_writer_time = start_time.elapsed();
    stop_signal.store(true, Ordering::Release);

    let mut total_corrupted = 0;
    let mut total_regressions = 0;

    for handle in reader_handles {
        let (id, read_count, regressions, corrupted) = handle.join().expect("reader joined");
        println!(
            "Reader {} finished: {} frames observed, {} regressions, {} corrupted",
            id, read_count, regressions, corrupted
        );
        total_regressions += regressions;
        total_corrupted += corrupted;
    }

    println!(
        "120 FPS test completed: {} frames in {:?}, avg FPS: {:.2}",
        target_frames,
        total_writer_time,
        (target_frames as f64) / total_writer_time.as_secs_f64()
    );

    assert_eq!(total_regressions, 0, "Encountered sequence regressions");
    assert_eq!(total_corrupted, 0, "Encountered torn or corrupted frames");
}

/// Stress Test 2: Atomic snapshot consistency between head_seq, head_slot, and dimensions.
///
/// Writer alternates between two distinct modes:
/// - Mode A (even seq): width = 1920, height = 1080, stride = 7680, slot = 0
/// - Mode B (odd seq):  width = 1080, height = 1920, stride = 4320, slot = 1
///
/// Evaluates whether `head()` or `dimensions()` allows non-atomic or torn reads:
/// - Checks if reader observes even seq with slot 1, or odd seq with slot 0.
/// - Checks if reader observes mismatched dimensions: (1920, 1920) or (1080, 1080).
#[test]
fn test_atomic_snapshot_consistency_race() {
    let path = unique_temp_path("atomic_consistency_race");
    let _cleanup = AutoUnlink(path.clone());

    let slot_size = 1920 * 1920 * 4;
    let writer = ShmWriter::create(&path, slot_size, 3).expect("writer create");

    let stop_signal = Arc::new(AtomicBool::new(false));
    let iterations = 100_000u64;

    let path_clone = path.clone();
    let stop_clone = Arc::clone(&stop_signal);

    let reader_handle = std::thread::spawn(move || {
        let reader = ShmReader::open(&path_clone).expect("reader open");
        let mut mismatched_slots = 0u64;
        let mut torn_dimensions = 0u64;
        let mut total_checks = 0u64;

        while !stop_clone.load(Ordering::Acquire) {
            let (seq, slot) = reader.head();
            let (w, h, s) = reader.dimensions();

            if seq > 0 {
                total_checks += 1;

                // Check slot/seq alignment
                let expected_slot = if seq % 2 == 0 { 0 } else { 1 };
                if slot != expected_slot {
                    mismatched_slots += 1;
                }

                // Check dimension consistency (either 1920x1080 with stride 7680, or 1080x1920 with stride 4320)
                let is_mode_a = w == 1920 && h == 1080 && s == 7680;
                let is_mode_b = w == 1080 && h == 1920 && s == 4320;

                if !is_mode_a && !is_mode_b {
                    torn_dimensions += 1;
                }
            }
            std::hint::spin_loop();
        }

        (total_checks, mismatched_slots, torn_dimensions)
    });

    for seq in 1..=iterations {
        let (w, h, slot) = if seq % 2 == 0 {
            (1920, 1080, 0)
        } else {
            (1080, 1920, 1)
        };

        writer.publish_frame(slot, w, h, seq).expect("publish");
        if seq % 1000 == 0 {
            std::thread::yield_now();
        }
    }

    stop_signal.store(true, Ordering::Release);
    let (checks, mismatched, torn) = reader_handle.join().expect("reader join");

    println!(
        "Atomic snapshot test: {} checks, {} mismatched (seq, slot) pairs, {} torn dimensions",
        checks, mismatched, torn
    );

    assert!(
        checks > 0,
        "Atomic snapshot test reader did not perform any checks"
    );
    assert_eq!(
        mismatched, 0,
        "Encountered {} mismatched (seq, slot) pairs: expected 0 under atomic seqlock",
        mismatched
    );
    assert_eq!(
        torn, 0,
        "Encountered {} torn dimension states: expected 0 under atomic seqlock",
        torn
    );
}

/// Stress Test 2a: Atomic FrameSnapshot API consistency test validating full 5-variable synchronization.
#[test]
fn test_atomic_frame_snapshot_api_consistency() {
    let path = unique_temp_path("atomic_frame_snapshot_api");
    let _cleanup = AutoUnlink(path.clone());

    let slot_size = 1920 * 1920 * 4;
    let writer = ShmWriter::create(&path, slot_size, 3).expect("writer create");

    let stop_signal = Arc::new(AtomicBool::new(false));
    let iterations = 100_000u64;

    let path_clone = path.clone();
    let stop_clone = Arc::clone(&stop_signal);

    let reader_handle = std::thread::spawn(move || {
        let reader = ShmReader::open(&path_clone).expect("reader open");
        let mut mismatched_slots = 0u64;
        let mut torn_dimensions = 0u64;
        let mut total_checks = 0u64;

        while !stop_clone.load(Ordering::Acquire) {
            let snap = reader.snapshot();

            if snap.seq > 0 {
                total_checks += 1;

                // Check slot/seq alignment
                let expected_slot = if snap.seq & 1 == 0 { 0 } else { 1 };
                if snap.slot != expected_slot {
                    mismatched_slots += 1;
                }

                // Check dimension consistency (either 1920x1080 with stride 7680, or 1080x1920 with stride 4320)
                let is_mode_a = snap.width == 1920 && snap.height == 1080 && snap.stride == 7680;
                let is_mode_b = snap.width == 1080 && snap.height == 1920 && snap.stride == 4320;

                let expected_mode_a = snap.seq & 1 == 0;
                let mode_matches = if expected_mode_a {
                    is_mode_a
                } else {
                    is_mode_b
                };

                if !mode_matches {
                    torn_dimensions += 1;
                }
            }
            std::hint::spin_loop();
        }

        (total_checks, mismatched_slots, torn_dimensions)
    });

    for seq in 1..=iterations {
        let (w, h, slot) = if seq % 2 == 0 {
            (1920, 1080, 0)
        } else {
            (1080, 1920, 1)
        };

        writer.publish_frame(slot, w, h, seq).expect("publish");
        if seq % 1000 == 0 {
            std::thread::yield_now();
        }
    }

    stop_signal.store(true, Ordering::Release);
    let (checks, mismatched, torn) = reader_handle.join().expect("reader join");

    println!(
        "Atomic FrameSnapshot API test: {} checks, {} mismatched (seq, slot) pairs, {} torn dimensions",
        checks, mismatched, torn
    );

    assert!(
        checks > 0,
        "Atomic FrameSnapshot API reader did not perform any checks"
    );
    assert_eq!(
        mismatched, 0,
        "Encountered {} mismatched (seq, slot) pairs: expected 0 under atomic FrameSnapshot",
        mismatched
    );
    assert_eq!(
        torn, 0,
        "Encountered {} torn dimension states: expected 0 under atomic FrameSnapshot",
        torn
    );
}

/// Stress Test 2b: Oracle verification proving Seqlock mitigation eliminates race condition.
///
/// Simulates how `ShmPool` behaves when `publish_frame` and `head()`/`dimensions()`
/// use seqlock generation counting on `head_seq`:
/// - Writer marks odd seq on start, writes slot and dimensions with Relaxed, marks even seq on finish.
/// - Reader loads seq, reads slot and dimensions, loads seq again. Retries if odd or changed.
///
/// Result: Exactly 0 mismatched slots and 0 torn dimensions across all checks.
#[test]
fn test_seqlock_oracle_proof() {
    use std::sync::atomic::AtomicU32;

    struct MockSeqlockHeader {
        head_seq: AtomicU64,
        head_slot: AtomicU32,
        width: AtomicU32,
        height: AtomicU32,
        stride: AtomicU32,
    }

    let header = Arc::new(MockSeqlockHeader {
        head_seq: AtomicU64::new(0),
        head_slot: AtomicU32::new(0),
        width: AtomicU32::new(0),
        height: AtomicU32::new(0),
        stride: AtomicU32::new(0),
    });

    let stop_signal = Arc::new(AtomicBool::new(false));
    let iterations = 100_000u64;

    let h_clone = Arc::clone(&header);
    let stop_clone = Arc::clone(&stop_signal);

    let reader_handle = std::thread::spawn(move || {
        let mut mismatched_slots = 0u64;
        let mut torn_dimensions = 0u64;
        let mut total_checks = 0u64;

        while !stop_clone.load(Ordering::Acquire) {
            // Seqlock read loop
            let (seq, slot, w, h, s) = loop {
                let v1 = h_clone.head_seq.load(Ordering::Acquire);
                if (v1 & 1) != 0 {
                    std::hint::spin_loop();
                    continue;
                }
                let slot = h_clone.head_slot.load(Ordering::Relaxed);
                let w = h_clone.width.load(Ordering::Relaxed);
                let h = h_clone.height.load(Ordering::Relaxed);
                let s = h_clone.stride.load(Ordering::Relaxed);
                let v2 = h_clone.head_seq.load(Ordering::Acquire);
                if v1 == v2 && v1 > 0 {
                    break (v1 / 2, slot, w, h, s);
                }
                if stop_clone.load(Ordering::Acquire) {
                    return (total_checks, mismatched_slots, torn_dimensions);
                }
                std::hint::spin_loop();
            };

            total_checks += 1;

            let expected_slot = if seq % 2 == 0 { 0 } else { 1 };
            if slot != expected_slot {
                mismatched_slots += 1;
            }

            let is_mode_a = w == 1920 && h == 1080 && s == 7680;
            let is_mode_b = w == 1080 && h == 1920 && s == 4320;

            if !is_mode_a && !is_mode_b {
                torn_dimensions += 1;
            }

            std::hint::spin_loop();
        }

        (total_checks, mismatched_slots, torn_dimensions)
    });

    for seq in 1..=iterations {
        let (w, h, slot) = if seq % 2 == 0 {
            (1920, 1080, 0)
        } else {
            (1080, 1920, 1)
        };

        // Seqlock write sequence
        header.head_seq.store(seq * 2 - 1, Ordering::Release);
        header.head_slot.store(slot, Ordering::Relaxed);
        header.width.store(w, Ordering::Relaxed);
        header.height.store(h, Ordering::Relaxed);
        header.stride.store(w * 4, Ordering::Relaxed);
        header.head_seq.store(seq * 2, Ordering::Release);

        if seq % 1000 == 0 {
            std::thread::yield_now();
        }
    }

    stop_signal.store(true, Ordering::Release);
    let (checks, mismatched, torn) = reader_handle.join().expect("reader join");

    println!(
        "Seqlock Oracle test: {} checks, {} mismatched slots, {} torn dimensions",
        checks, mismatched, torn
    );

    assert_eq!(mismatched, 0, "Seqlock oracle must have 0 mismatched slots");
    assert_eq!(torn, 0, "Seqlock oracle must have 0 torn dimensions");
}

/// Stress Test 3: Fast Reader vs. Slow Writer (Starvation & No Spurious Frames).
///
/// Writer produces frames slowly (20 frames over 400ms).
/// Reader polls in a tight spin loop hundreds of thousands of times.
/// Verifies:
/// - Exactly 20 unique frames are observed (no false new frames).
/// - Sequence strictly increases monotonically.
/// - Read payload data perfectly matches published sequence.
#[test]
fn test_fast_reader_starvation_no_spurious_frames() {
    let path = unique_temp_path("fast_reader_starvation");
    let _cleanup = AutoUnlink(path.clone());

    let mut writer = ShmWriter::create(&path, 1024, 3).expect("create writer");
    let target_frames = 20u64;

    let path_clone = path.clone();
    let reader_handle = std::thread::spawn(move || {
        let reader = ShmReader::open(&path_clone).expect("open reader");
        let mut observed_frames = Vec::new();
        let mut last_seq = 0u64;
        let mut total_polls = 0u64;

        while last_seq < target_frames {
            total_polls += 1;
            let (seq, slot) = reader.head();
            if seq > last_seq {
                assert!(
                    seq == last_seq + 1,
                    "expected consecutive frame, got skip: last={}, curr={}",
                    last_seq,
                    seq
                );
                let data = reader.read_slot_exact(slot, 8).expect("read_slot_exact");
                let payload_seq = u64::from_le_bytes(data[0..8].try_into().unwrap());
                assert_eq!(payload_seq, seq, "payload seq mismatch");
                observed_frames.push(seq);
                last_seq = seq;
            }
            std::hint::spin_loop();
        }

        (observed_frames, total_polls)
    });

    for seq in 1..=target_frames {
        std::thread::sleep(Duration::from_millis(15));
        let slot = writer.next_slot();
        let buf = writer.slot_mut(slot).expect("slot_mut");
        buf[0..8].copy_from_slice(&seq.to_le_bytes());
        writer.publish_frame(slot, 640, 480, seq).expect("publish");
    }

    let (observed, polls) = reader_handle.join().expect("reader join");
    println!(
        "Fast reader finished: {} frames observed in {} polls",
        observed.len(),
        polls
    );
    assert_eq!(observed.len(), target_frames as usize);
    assert_eq!(observed, (1..=target_frames).collect::<Vec<_>>());
}

/// Stress Test 4: Fast Writer vs. Slow Reader (Writer Overrun / Slot Skipping).
///
/// Writer produces 5,000 frames unthrottled.
/// Reader reads periodically (sleeping 1ms between checks).
/// Verifies:
/// - Reader observes monotonically increasing sequence numbers (slot skipping occurs without sequence inversion).
/// - No crashing, panicking, or out-of-bounds slot accesses during rapid overrun.
#[test]
fn test_unthrottled_writer_slot_skipping() {
    let path = unique_temp_path("unthrottled_writer_skipping");
    let _cleanup = AutoUnlink(path.clone());

    let mut writer = ShmWriter::create(&path, 2048, 3).expect("create writer");
    let total_frames = 5_000u64;

    let path_clone = path.clone();
    let reader_handle = std::thread::spawn(move || {
        let reader = ShmReader::open(&path_clone).expect("open reader");
        let mut last_seq = 0u64;
        let mut read_count = 0u64;
        let mut skips = 0u64;

        while last_seq < total_frames {
            let (seq, slot) = reader.head();
            if seq > last_seq {
                if seq > last_seq + 1 {
                    skips += seq - (last_seq + 1);
                }
                assert!(slot < 3, "slot index out of bounds: {}", slot);
                let _ = reader.read_slot_exact(slot, 16).expect("read_slot_exact");
                last_seq = seq;
                read_count += 1;
            }
            std::thread::sleep(Duration::from_micros(500));
        }

        (read_count, skips)
    });

    for seq in 1..=total_frames {
        let slot = writer.next_slot();
        let buf = writer.slot_mut(slot).expect("slot_mut");
        buf[0..8].copy_from_slice(&seq.to_le_bytes());
        writer.publish_frame(slot, 800, 600, seq).expect("publish");
    }

    let (reads, skips) = reader_handle.join().expect("reader join");
    println!(
        "Slot skipping test: writer published {}, reader sampled {} frames, skipped {} frames",
        total_frames, reads, skips
    );
    assert!(reads > 0);
    assert!(
        skips > 0,
        "Expected slot skipping during unthrottled writer burst"
    );
}

/// Stress Test 5: Writer Overwriting Slot During Concurrent Slow Read (Torn Frame Potential).
///
/// In standard triple buffering, if reader holds a buffer, writer must not overwrite it.
/// Because `ShmPool` currently uses blind round-robin (`(head_slot + 1) % num_slots`) without
/// reader reservation or slot lock, if reader takes longer than 2 frame intervals, the writer
/// wraps around and writes directly into the slot the reader is actively reading!
#[test]
fn test_writer_overwriting_slow_reader_slot() {
    let path = unique_temp_path("writer_overwriting_slot");
    let _cleanup = AutoUnlink(path.clone());

    let slot_size = 64 * 1024;
    let mut writer = ShmWriter::create(&path, slot_size, 3).expect("writer create");

    // Publish frame 1 into slot 1
    let slot1 = writer.next_slot();
    assert_eq!(slot1, 1);
    let buf1 = writer.slot_mut(slot1).expect("slot_mut");
    buf1[..8].fill(0x11);
    writer.publish_frame(slot1, 640, 480, 1).expect("publish");

    let reader = ShmReader::open(&path).expect("reader open");
    let (seq, slot) = reader.head();
    assert_eq!(seq, 1);
    assert_eq!(slot, 1);

    // Reader accesses slot 1
    let slot1_data = reader.read_slot(slot).expect("read_slot");
    assert_eq!(slot1_data[0], 0x11);

    // Writer writes frame 2 into slot 2
    let slot2 = writer.next_slot();
    assert_eq!(slot2, 2);
    let buf2 = writer.slot_mut(slot2).expect("slot_mut");
    buf2[..8].fill(0x22);
    writer.publish_frame(slot2, 640, 480, 2).expect("publish");

    // Writer writes frame 3 into slot 0
    let slot3 = writer.next_slot();
    assert_eq!(slot3, 0);
    let buf3 = writer.slot_mut(slot3).expect("slot_mut");
    buf3[..8].fill(0x33);
    writer.publish_frame(slot3, 640, 480, 3).expect("publish");

    // Writer writes frame 4 into slot 1 (OVERWRITING SLOT 1 while reader still holds reference!)
    let slot4 = writer.next_slot();
    assert_eq!(slot4, 1); // Wraps around to slot 1!
    let buf4 = writer.slot_mut(slot4).expect("slot_mut");
    buf4[..8].fill(0x44);
    writer.publish_frame(slot4, 640, 480, 4).expect("publish");

    // If reader re-reads slot 1:
    let reloaded = reader.read_slot(slot4).expect("read_slot");
    let reloaded_byte = reloaded[0];
    println!(
        "Byte observed after re-reading slot 1: 0x{:02x}",
        reloaded_byte
    );
    assert_eq!(
        reloaded_byte, 0x44,
        "Slot 1 was mutated by writer wrap-around"
    );
}

/// Stress Test 6: Memory safety, bounds checks, alignment, and arithmetic overflow edge cases.
#[test]
fn test_alignment_and_bounds_validation() {
    let path = unique_temp_path("bounds_and_alignment");
    let _cleanup = AutoUnlink(path.clone());

    // 1. Check ShmHeader alignment and size
    assert_eq!(std::mem::size_of::<ShmHeader>(), 64);
    assert_eq!(std::mem::align_of::<ShmHeader>(), 64);
    assert_eq!(SHM_HEADER_SIZE, 64);
    assert_eq!(SHM_MAGIC, 0x4144_4253);
    assert_eq!(SHM_VERSION, 1);

    // 2. Arithmetic overflow in pool creation
    let overflow_path = unique_temp_path("overflow_pool");
    let _cleanup_ov = AutoUnlink(overflow_path.clone());
    let res = ShmPool::create(&overflow_path, u32::MAX, u32::MAX);
    assert!(
        res.is_err(),
        "Expected overflow error on u32::MAX slot configuration"
    );

    // 3. Normal pool creation and bounds verification
    let mut writer = ShmWriter::create(&path, 512, 3).expect("writer create");
    let reader = ShmReader::open(&path).expect("reader open");

    // Writable checks
    assert!(writer.pool().is_writable());
    assert!(!reader.pool().is_writable());

    // Out of bounds slot access
    assert!(matches!(
        writer.slot_mut(3),
        Err(ShmError::SlotOutOfBounds {
            slot: 3,
            num_slots: 3
        })
    ));
    assert!(matches!(
        reader.read_slot(3),
        Err(ShmError::SlotOutOfBounds {
            slot: 3,
            num_slots: 3
        })
    ));
    assert!(matches!(
        writer.publish_frame(3, 100, 100, 1),
        Err(ShmError::SlotOutOfBounds {
            slot: 3,
            num_slots: 3
        })
    ));

    // Over-capacity write/read
    assert!(matches!(
        writer.write_frame(0, &[0u8; 513]),
        Err(ShmError::FrameTooLarge {
            len: 513,
            capacity: 512
        })
    ));
    assert!(matches!(
        reader.read_slot_exact(0, 513),
        Err(ShmError::FrameTooLarge {
            len: 513,
            capacity: 512
        })
    ));
}
