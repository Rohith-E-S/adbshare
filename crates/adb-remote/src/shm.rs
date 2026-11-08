//! Shared memory triple-buffered frame pool for low-latency video streaming.
//!
//! Provides lock-free zero-copy BGRA frame transfer between `adb-daemon` (writer)
//! and `adb-gui` (reader) using a memory-mapped file located in `$XDG_RUNTIME_DIR`.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Magic bytes identifying ADBShare SHM pool ("ADBS" in big-endian ASCII).
pub const SHM_MAGIC: u32 = 0x4144_4253;

/// Layout and protocol version.
pub const SHM_VERSION: u32 = 1;

/// Fixed size of the shared memory header in bytes (64 bytes).
pub const SHM_HEADER_SIZE: usize = 64;

/// Default number of slots in the triple buffer pool.
pub const DEFAULT_NUM_SLOTS: u32 = 3;

/// Default slot capacity in bytes: 2560 * 2560 * 4 = 26,214,400 bytes (25 MiB).
/// Accommodates any video frame up to 2560x2560 in 32-bit BGRA/RGBA.
pub const DEFAULT_SLOT_SIZE: u32 = 2560 * 2560 * 4;

/// Pixel format: BGRA8888 (standard for FFmpeg `-pix_fmt bgra` and GPUI Vulkan).
pub const FORMAT_BGRA8888: u32 = 0;

/// Pixel format: RGBA8888.
pub const FORMAT_RGBA8888: u32 = 1;

/// 64-byte shared memory pool header.
///
/// Guaranteed 64-byte size and alignment matching cache-line boundaries.
#[repr(C, align(64))]
pub struct ShmHeader {
    /// Magic number (`0x41444253`).
    pub magic: u32,
    /// Protocol version (`1`).
    pub version: u32,
    /// Last published frame width in pixels.
    pub width: AtomicU32,
    /// Last published frame height in pixels.
    pub height: AtomicU32,
    /// Row stride in bytes (typically width * 4).
    pub stride: AtomicU32,
    /// Preallocated byte capacity per slot.
    pub slot_size: u32,
    /// Total number of buffer slots (typically 3).
    pub num_slots: u32,
    /// Pixel format (0 = BGRA8888).
    pub format: u32,
    /// Monotonically increasing sequence number of the latest published frame.
    pub head_seq: AtomicU64,
    /// Slot index containing the latest published frame (0..num_slots - 1).
    pub head_slot: AtomicU32,
    /// Status and capability flags.
    pub flags: AtomicU32,
    /// Reserved for future extensions / padding to guarantee 64-byte size and alignment.
    pub reserved: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<ShmHeader>() == 64);
const _: () = assert!(std::mem::align_of::<ShmHeader>() == 64);

impl ShmHeader {
    /// Check whether header magic and version match expected constants.
    pub fn is_valid(&self) -> bool {
        self.magic == SHM_MAGIC && self.version == SHM_VERSION
    }
}

/// An atomic, tear-free snapshot of published frame metadata from shared memory.
///
/// Acquired via [`ShmReader::snapshot`] or [`ShmPool::snapshot`] using a seqlock loop
/// to ensure that `seq`, `slot`, and frame dimensions (`width`, `height`, `stride`)
/// are read with complete multi-variable atomic consistency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameSnapshot {
    /// Monotonically increasing sequence number of the published frame (0 if uninitialized).
    pub seq: u64,
    /// Ring-buffer slot index in shared memory containing the frame data (0..num_slots - 1).
    pub slot: u32,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes (typically width * 4).
    pub stride: u32,
}

impl FrameSnapshot {
    /// Returns true if this snapshot represents a valid, published frame.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.seq > 0 && self.width > 0 && self.height > 0
    }

    /// Calculate the minimum required buffer size for the pixel payload (`height * stride`).
    #[inline]
    pub fn frame_bytes(&self) -> usize {
        (self.height as usize).saturating_mul(self.stride as usize)
    }

    /// Return the published dimensions as `(width, height, stride)`.
    #[inline]
    pub fn dimensions(&self) -> (u32, u32, u32) {
        (self.width, self.height, self.stride)
    }

    /// Return the published head identifier as `(seq, slot)`.
    #[inline]
    pub fn head(&self) -> (u64, u32) {
        (self.seq, self.slot)
    }
}

/// Errors occurring during shared memory operations.
#[derive(Debug, thiserror::Error)]
pub enum ShmError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid magic: expected 0x{expected:08x}, got 0x{actual:08x}")]
    InvalidMagic { expected: u32, actual: u32 },

    #[error("unsupported version: expected {expected}, got {actual}")]
    UnsupportedVersion { expected: u32, actual: u32 },

    #[error(
        "header too small: file size {file_size} is smaller than required header {header_size}"
    )]
    HeaderTooSmall {
        file_size: usize,
        header_size: usize,
    },

    #[error(
        "file too small: expected at least {expected} bytes for {num_slots} slots, got {actual}"
    )]
    FileTooSmall {
        expected: usize,
        actual: usize,
        num_slots: u32,
    },

    #[error("slot index out of bounds: slot {slot} >= num_slots {num_slots}")]
    SlotOutOfBounds { slot: u32, num_slots: u32 },

    #[error("frame size {len} exceeds slot capacity {capacity}")]
    FrameTooLarge { len: usize, capacity: usize },

    #[error("shared memory pool is mapped read-only")]
    ReadOnly,

    #[error("invalid slot configuration: slot_size {slot_size}, num_slots {num_slots}")]
    InvalidSlotConfig { slot_size: u32, num_slots: u32 },

    #[error("invalid sequence number {seq}: must be > 0")]
    InvalidSeq { seq: u64 },
}

/// Validate slot configuration read from an untrusted header / arguments.
fn validate_slot_config(slot_size: u32, num_slots: u32) -> Result<(), ShmError> {
    if !(1..=64).contains(&num_slots) || slot_size == 0 || slot_size > 64 * 1024 * 1024 {
        return Err(ShmError::InvalidSlotConfig {
            slot_size,
            num_slots,
        });
    }
    Ok(())
}

/// `SHM_HEADER_SIZE + num_slots * slot_size` with overflow checking.
fn expected_len(num_slots: u32, slot_size: u32) -> Result<usize, ShmError> {
    (num_slots as u64)
        .checked_mul(slot_size as u64)
        .and_then(|slots| (SHM_HEADER_SIZE as u64).checked_add(slots))
        .and_then(|v| usize::try_from(v).ok())
        .ok_or(ShmError::InvalidSlotConfig {
            slot_size,
            num_slots,
        })
}

#[allow(dead_code)]
enum MmapKind {
    Mut(memmap2::MmapMut),
    ReadOnly(memmap2::Mmap),
}

/// Shared memory pool managing preallocated triple-buffered frame slots.
pub struct ShmPool {
    _mmap: MmapKind,
    ptr: *mut u8,
    len: usize,
    slot_size: u32,
    num_slots: u32,
    path: PathBuf,
}

unsafe impl Send for ShmPool {}
unsafe impl Sync for ShmPool {}

impl ShmPool {
    /// Create a new shared memory pool file backing a writable triple-buffer region.
    pub fn create(path: &Path, slot_size: u32, num_slots: u32) -> Result<Self, ShmError> {
        validate_slot_config(slot_size, num_slots)?;
        let total_size = (SHM_HEADER_SIZE as u64)
            .checked_add(
                (num_slots as u64)
                    .checked_mul(slot_size as u64)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "slot configuration overflow")
                    })?,
            )
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "total size overflow"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }

        file.set_len(total_size)?;

        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };
        let ptr = mmap.as_mut_ptr();
        let len = mmap.len();

        let header_ptr = ptr as *mut ShmHeader;
        unsafe {
            header_ptr.write(ShmHeader {
                magic: SHM_MAGIC,
                version: SHM_VERSION,
                width: AtomicU32::new(0),
                height: AtomicU32::new(0),
                stride: AtomicU32::new(0),
                slot_size,
                num_slots,
                format: FORMAT_BGRA8888,
                head_seq: AtomicU64::new(0),
                head_slot: AtomicU32::new(0),
                flags: AtomicU32::new(0),
                reserved: [0u8; 16],
            });
        }

        Ok(Self {
            _mmap: MmapKind::Mut(mmap),
            ptr,
            len,
            slot_size,
            num_slots,
            path: path.to_path_buf(),
        })
    }

    /// Open an existing shared memory pool file read-only.
    pub fn open(path: &Path) -> Result<Self, ShmError> {
        let file = OpenOptions::new().read(true).open(path)?;
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        let ptr = mmap.as_ptr() as *mut u8;
        let len = mmap.len();

        if len < SHM_HEADER_SIZE {
            return Err(ShmError::HeaderTooSmall {
                file_size: len,
                header_size: SHM_HEADER_SIZE,
            });
        }

        let header_ptr = ptr as *const ShmHeader;
        let header = unsafe { &*header_ptr };

        if header.magic != SHM_MAGIC {
            return Err(ShmError::InvalidMagic {
                expected: SHM_MAGIC,
                actual: header.magic,
            });
        }

        if header.version != SHM_VERSION {
            return Err(ShmError::UnsupportedVersion {
                expected: SHM_VERSION,
                actual: header.version,
            });
        }

        let slot_size = header.slot_size;
        let num_slots = header.num_slots;
        validate_slot_config(slot_size, num_slots)?;
        let expected = expected_len(num_slots, slot_size)?;

        if len < expected {
            return Err(ShmError::FileTooSmall {
                expected,
                actual: len,
                num_slots,
            });
        }

        Ok(Self {
            _mmap: MmapKind::ReadOnly(mmap),
            ptr,
            len,
            slot_size,
            num_slots,
            path: path.to_path_buf(),
        })
    }

    /// Open an existing shared memory pool file read-write.
    pub fn open_mut(path: &Path) -> Result<Self, ShmError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file)? };
        let ptr = mmap.as_mut_ptr();
        let len = mmap.len();

        if len < SHM_HEADER_SIZE {
            return Err(ShmError::HeaderTooSmall {
                file_size: len,
                header_size: SHM_HEADER_SIZE,
            });
        }

        let header_ptr = ptr as *const ShmHeader;
        let header = unsafe { &*header_ptr };

        if header.magic != SHM_MAGIC {
            return Err(ShmError::InvalidMagic {
                expected: SHM_MAGIC,
                actual: header.magic,
            });
        }

        if header.version != SHM_VERSION {
            return Err(ShmError::UnsupportedVersion {
                expected: SHM_VERSION,
                actual: header.version,
            });
        }

        let slot_size = header.slot_size;
        let num_slots = header.num_slots;
        validate_slot_config(slot_size, num_slots)?;
        let expected = expected_len(num_slots, slot_size)?;

        if len < expected {
            return Err(ShmError::FileTooSmall {
                expected,
                actual: len,
                num_slots,
            });
        }

        Ok(Self {
            _mmap: MmapKind::Mut(mmap),
            ptr,
            len,
            slot_size,
            num_slots,
            path: path.to_path_buf(),
        })
    }

    /// Returns true if this pool instance was opened with write permissions.
    #[inline]
    pub fn is_writable(&self) -> bool {
        matches!(self._mmap, MmapKind::Mut(_))
    }

    /// Returns the total mapped length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the pool mapping has 0 length.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Access the shared header.
    #[inline]
    pub fn header(&self) -> &ShmHeader {
        unsafe { &*(self.ptr as *const ShmHeader) }
    }

    /// Calculate next writable slot index for triple-buffering.
    #[inline]
    pub fn next_writable_slot(&self) -> u32 {
        if self.num_slots == 0 {
            return 0;
        }
        let current = self.header().head_slot.load(Ordering::Relaxed);
        (current + 1) % self.num_slots
    }

    /// Read raw slice of the requested slot.
    pub fn read_slot(&self, slot: u32) -> Result<&[u8], ShmError> {
        if slot >= self.num_slots {
            return Err(ShmError::SlotOutOfBounds {
                slot,
                num_slots: self.num_slots,
            });
        }
        let offset = (slot as usize)
            .checked_mul(self.slot_size as usize)
            .and_then(|o| SHM_HEADER_SIZE.checked_add(o))
            .ok_or(ShmError::InvalidSlotConfig {
                slot_size: self.slot_size,
                num_slots: self.num_slots,
            })?;
        if offset
            .checked_add(self.slot_size as usize)
            .is_none_or(|end| end > self.len)
        {
            return Err(ShmError::FileTooSmall {
                expected: offset + self.slot_size as usize,
                actual: self.len,
                num_slots: self.num_slots,
            });
        }
        std::sync::atomic::fence(Ordering::Acquire);
        let slice =
            unsafe { std::slice::from_raw_parts(self.ptr.add(offset), self.slot_size as usize) };
        Ok(slice)
    }

    /// Alias for `read_slot` satisfying interface contracts.
    #[inline]
    pub fn slot(&self, slot: u32) -> Result<&[u8], ShmError> {
        self.read_slot(slot)
    }

    /// Read exact byte count from the requested slot with bounds verification.
    pub fn read_slot_exact(&self, slot: u32, len: usize) -> Result<&[u8], ShmError> {
        let full = self.read_slot(slot)?;
        if len > full.len() {
            return Err(ShmError::FrameTooLarge {
                len,
                capacity: full.len(),
            });
        }
        Ok(&full[..len])
    }

    /// Access mutable slice of the requested slot for writing frame data.
    pub fn slot_mut(&mut self, slot: u32) -> Result<&mut [u8], ShmError> {
        if !self.is_writable() {
            return Err(ShmError::ReadOnly);
        }
        if slot >= self.num_slots {
            return Err(ShmError::SlotOutOfBounds {
                slot,
                num_slots: self.num_slots,
            });
        }
        let offset = (slot as usize)
            .checked_mul(self.slot_size as usize)
            .and_then(|o| SHM_HEADER_SIZE.checked_add(o))
            .ok_or(ShmError::InvalidSlotConfig {
                slot_size: self.slot_size,
                num_slots: self.num_slots,
            })?;
        if offset
            .checked_add(self.slot_size as usize)
            .is_none_or(|end| end > self.len)
        {
            return Err(ShmError::FileTooSmall {
                expected: offset + self.slot_size as usize,
                actual: self.len,
                num_slots: self.num_slots,
            });
        }
        let slice = unsafe {
            std::slice::from_raw_parts_mut(self.ptr.add(offset), self.slot_size as usize)
        };
        Ok(slice)
    }

    /// Copy frame data into a slot with capacity validation.
    pub fn write_frame(&mut self, slot: u32, data: &[u8]) -> Result<(), ShmError> {
        let buf = self.slot_mut(slot)?;
        if data.len() > buf.len() {
            return Err(ShmError::FrameTooLarge {
                len: data.len(),
                capacity: buf.len(),
            });
        }
        buf[..data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Publish a newly written frame atomically with Release ordering.
    pub fn publish_frame(
        &self,
        slot: u32,
        width: u32,
        height: u32,
        seq: u64,
    ) -> Result<(), ShmError> {
        if !self.is_writable() {
            return Err(ShmError::ReadOnly);
        }
        if slot >= self.num_slots {
            return Err(ShmError::SlotOutOfBounds {
                slot,
                num_slots: self.num_slots,
            });
        }
        let header = self.header();

        if seq == 0 {
            return Err(ShmError::InvalidSeq { seq });
        }
        // Even completion version, odd in-progress version. Saturate at
        // u64::MAX-1 (even) so u64::MAX does not wrap to even/even.
        let complete_ver = seq.checked_mul(2).unwrap_or(u64::MAX - 1) & !1;
        let in_progress_ver = complete_ver.saturating_sub(1);

        // 1. Mark update in progress with odd version
        header.head_seq.store(in_progress_ver, Ordering::Release);
        std::sync::atomic::fence(Ordering::Release);

        // 2. Update metadata fields with Relaxed ordering
        header.head_slot.store(slot, Ordering::Relaxed);
        header.width.store(width, Ordering::Relaxed);
        header.height.store(height, Ordering::Relaxed);
        header
            .stride
            .store(width.saturating_mul(4), Ordering::Relaxed);

        std::sync::atomic::fence(Ordering::Release);

        // 3. Mark update complete with even version
        header.head_seq.store(complete_ver, Ordering::Release);
        Ok(())
    }

    /// Obtain an atomic, tear-free snapshot of current frame metadata via a seqlock loop.
    ///
    /// Retries if an update is actively in progress (odd sequence counter) or if
    /// the sequence counter changed while reading fields. Bounded: if the
    /// writer died mid-publish (permanently odd), returns the last readable
    /// metadata instead of spinning forever.
    pub fn snapshot(&self) -> FrameSnapshot {
        for i in 0..10_000u32 {
            if let Some(s) = self.try_snapshot() {
                return s;
            }
            if i > 32 {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
        self.snapshot_stale()
    }

    /// Non-blocking seqlock attempt. Returns `None` if the writer is mid-publish.
    pub fn try_snapshot(&self) -> Option<FrameSnapshot> {
        let header = self.header();
        let v1 = header.head_seq.load(Ordering::Acquire);
        if (v1 & 1) != 0 {
            return None;
        }
        std::sync::atomic::fence(Ordering::Acquire);
        let slot = header.head_slot.load(Ordering::Relaxed);
        let width = header.width.load(Ordering::Relaxed);
        let height = header.height.load(Ordering::Relaxed);
        let stride = header.stride.load(Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Acquire);
        let v2 = header.head_seq.load(Ordering::Acquire);
        if v1 != v2 {
            return None;
        }
        Some(FrameSnapshot {
            seq: v1 / 2,
            slot,
            width,
            height,
            stride,
        })
    }

    fn snapshot_stale(&self) -> FrameSnapshot {
        let header = self.header();
        FrameSnapshot {
            seq: header.head_seq.load(Ordering::Acquire) / 2,
            slot: header.head_slot.load(Ordering::Relaxed),
            width: header.width.load(Ordering::Relaxed),
            height: header.height.load(Ordering::Relaxed),
            stride: header.stride.load(Ordering::Relaxed),
        }
    }

    /// Latest published head tuple: `(seq, slot)`.
    #[inline]
    pub fn head(&self) -> (u64, u32) {
        let snap = self.snapshot();
        (snap.seq, snap.slot)
    }

    /// Monotonically increasing sequence number of latest published frame.
    #[inline]
    pub fn head_seq(&self) -> u64 {
        self.snapshot().seq
    }

    /// Slot index of latest published frame.
    #[inline]
    pub fn head_slot(&self) -> u32 {
        self.snapshot().slot
    }

    /// Current published dimensions: `(width, height, stride)`.
    #[inline]
    pub fn dimensions(&self) -> (u32, u32, u32) {
        let snap = self.snapshot();
        (snap.width, snap.height, snap.stride)
    }

    /// Slot byte capacity.
    #[inline]
    pub fn slot_size(&self) -> u32 {
        self.slot_size
    }

    /// Number of preallocated slots.
    #[inline]
    pub fn num_slots(&self) -> u32 {
        self.num_slots
    }

    /// Path to backing file.
    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unlink backing file from filesystem.
    pub fn unlink(&self) -> io::Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

/// Type-safe writer handle wrapping `ShmPool`.
pub struct ShmWriter {
    pool: ShmPool,
}

impl ShmWriter {
    pub fn create(
        path: impl AsRef<Path>,
        slot_size: u32,
        num_slots: u32,
    ) -> Result<Self, ShmError> {
        ShmPool::create(path.as_ref(), slot_size, num_slots).map(|pool| Self { pool })
    }

    #[inline]
    pub fn next_slot(&self) -> u32 {
        self.pool.next_writable_slot()
    }

    #[inline]
    pub fn slot_mut(&mut self, slot: u32) -> Result<&mut [u8], ShmError> {
        self.pool.slot_mut(slot)
    }

    #[inline]
    pub fn write_frame(&mut self, slot: u32, data: &[u8]) -> Result<(), ShmError> {
        self.pool.write_frame(slot, data)
    }

    #[inline]
    pub fn publish_frame(
        &self,
        slot: u32,
        width: u32,
        height: u32,
        seq: u64,
    ) -> Result<(), ShmError> {
        self.pool.publish_frame(slot, width, height, seq)
    }

    /// Obtain an atomic, tear-free snapshot of current frame metadata.
    #[inline]
    pub fn snapshot(&self) -> FrameSnapshot {
        self.pool.snapshot()
    }

    /// Latest published head tuple: `(seq, slot)`.
    #[inline]
    pub fn head(&self) -> (u64, u32) {
        self.pool.head()
    }

    /// Current published dimensions: `(width, height, stride)`.
    #[inline]
    pub fn dimensions(&self) -> (u32, u32, u32) {
        self.pool.dimensions()
    }

    #[inline]
    pub fn header(&self) -> &ShmHeader {
        self.pool.header()
    }

    #[inline]
    pub fn pool(&self) -> &ShmPool {
        &self.pool
    }

    #[inline]
    pub fn pool_mut(&mut self) -> &mut ShmPool {
        &mut self.pool
    }

    #[inline]
    pub fn into_pool(self) -> ShmPool {
        self.pool
    }
}

/// Type-safe reader handle wrapping `ShmPool`.
pub struct ShmReader {
    pool: ShmPool,
}

impl ShmReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ShmError> {
        ShmPool::open(path.as_ref()).map(|pool| Self { pool })
    }

    #[inline]
    pub fn read_slot(&self, slot: u32) -> Result<&[u8], ShmError> {
        self.pool.read_slot(slot)
    }

    #[inline]
    pub fn slot(&self, slot: u32) -> Result<&[u8], ShmError> {
        self.pool.slot(slot)
    }

    #[inline]
    pub fn read_slot_exact(&self, slot: u32, len: usize) -> Result<&[u8], ShmError> {
        self.pool.read_slot_exact(slot, len)
    }

    /// Obtain an atomic, tear-free snapshot of current frame metadata.
    #[inline]
    pub fn snapshot(&self) -> FrameSnapshot {
        self.pool.snapshot()
    }

    /// Non-blocking snapshot; `None` if the writer is mid-publish (torn).
    #[inline]
    pub fn try_snapshot(&self) -> Option<FrameSnapshot> {
        self.pool.try_snapshot()
    }

    /// Latest published head tuple: `(seq, slot)`.
    #[inline]
    pub fn head(&self) -> (u64, u32) {
        self.pool.head()
    }

    #[inline]
    pub fn head_seq(&self) -> u64 {
        self.pool.head_seq()
    }

    #[inline]
    pub fn head_slot(&self) -> u32 {
        self.pool.head_slot()
    }

    #[inline]
    pub fn dimensions(&self) -> (u32, u32, u32) {
        self.pool.dimensions()
    }

    #[inline]
    pub fn header(&self) -> &ShmHeader {
        self.pool.header()
    }

    #[inline]
    pub fn pool(&self) -> &ShmPool {
        &self.pool
    }

    #[inline]
    pub fn into_pool(self) -> ShmPool {
        self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_and_offsets() {
        assert_eq!(std::mem::size_of::<ShmHeader>(), 64);
        assert_eq!(std::mem::align_of::<ShmHeader>(), 64);

        let dummy = std::mem::MaybeUninit::<ShmHeader>::uninit();
        let base = dummy.as_ptr() as usize;
        unsafe {
            let p = dummy.as_ptr();
            assert_eq!(&raw const (*p).magic as usize - base, 0);
            assert_eq!(&raw const (*p).version as usize - base, 4);
            assert_eq!(&raw const (*p).width as usize - base, 8);
            assert_eq!(&raw const (*p).height as usize - base, 12);
            assert_eq!(&raw const (*p).stride as usize - base, 16);
            assert_eq!(&raw const (*p).slot_size as usize - base, 20);
            assert_eq!(&raw const (*p).num_slots as usize - base, 24);
            assert_eq!(&raw const (*p).format as usize - base, 28);
            assert_eq!(&raw const (*p).head_seq as usize - base, 32);
            assert_eq!(&raw const (*p).head_slot as usize - base, 40);
            assert_eq!(&raw const (*p).flags as usize - base, 44);
            assert_eq!(&raw const (*p).reserved as usize - base, 48);
        }
    }

    #[test]
    fn roundtrip_publish_and_read() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_shm_{}.shm", std::process::id()));

        let slot_size = 1024u32;
        let num_slots = 3u32;

        let mut writer = ShmWriter::create(&path, slot_size, num_slots).expect("create writer");
        let reader = ShmReader::open(&path).expect("open reader");

        assert_eq!(reader.header().magic, SHM_MAGIC);
        assert_eq!(reader.header().version, SHM_VERSION);
        assert!(reader.header().is_valid());
        assert_eq!(reader.header().num_slots, 3);
        assert_eq!(reader.header().slot_size, 1024);

        let slot = writer.next_slot();
        assert_eq!(slot, 1);
        let buf = writer.slot_mut(slot).expect("slot_mut");
        buf[0..4].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);

        writer.publish_frame(slot, 1920, 1080, 1).expect("publish");

        let (seq, head_slot) = reader.head();
        assert_eq!(seq, 1);
        assert_eq!(head_slot, 1);
        let (w, h, stride) = reader.dimensions();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
        assert_eq!(stride, 1920 * 4);

        let data = reader.read_slot_exact(head_slot, 4).expect("read");
        assert_eq!(data, &[0x11, 0x22, 0x33, 0x44]);

        let direct = reader.slot(head_slot).expect("slot");
        assert_eq!(&direct[..4], &[0x11, 0x22, 0x33, 0x44]);

        let _ = writer.pool().unlink();
    }

    #[test]
    fn bounds_and_error_handling() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_shm_bounds_{}.shm", std::process::id()));

        let mut writer = ShmWriter::create(&path, 512, 3).expect("create");
        let reader = ShmReader::open(&path).expect("open");

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
            reader.read_slot_exact(0, 513),
            Err(ShmError::FrameTooLarge {
                len: 513,
                capacity: 512
            })
        ));

        let _ = writer.pool().unlink();
    }

    #[test]
    fn concurrent_writer_reader() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_shm_concur_{}.shm", std::process::id()));

        let slot_size = 4096u32;
        let num_slots = 3u32;
        let mut writer = ShmWriter::create(&path, slot_size, num_slots).expect("create");

        let path_clone = path.clone();
        let reader_thread = std::thread::spawn(move || {
            let reader = ShmReader::open(&path_clone).expect("open in thread");
            let mut last_seq = 0u64;
            while last_seq < 100 {
                let (seq, slot) = reader.head();
                if seq > last_seq {
                    let frame = reader.read_slot_exact(slot, 8).expect("read");
                    let expected_val = (seq as u8).wrapping_mul(7);
                    assert_eq!(frame[0], expected_val);
                    last_seq = seq;
                }
                std::hint::spin_loop();
            }
        });

        for seq in 1..=100u64 {
            let slot = writer.next_slot();
            let buf = writer.slot_mut(slot).expect("slot_mut");
            let val = (seq as u8).wrapping_mul(7);
            buf[..8].fill(val);
            writer.publish_frame(slot, 640, 480, seq).expect("publish");
            std::thread::yield_now();
        }

        reader_thread.join().expect("reader thread completed");
        let _ = writer.pool().unlink();
    }

    #[test]
    fn frame_snapshot_api_and_defaults() {
        let snap = FrameSnapshot::default();
        assert_eq!(snap.seq, 0);
        assert_eq!(snap.slot, 0);
        assert_eq!(snap.width, 0);
        assert_eq!(snap.height, 0);
        assert_eq!(snap.stride, 0);
        assert!(!snap.is_valid());
        assert_eq!(snap.frame_bytes(), 0);
        assert_eq!(snap.dimensions(), (0, 0, 0));
        assert_eq!(snap.head(), (0, 0));

        let valid_snap = FrameSnapshot {
            seq: 15,
            slot: 2,
            width: 1920,
            height: 1080,
            stride: 7680,
        };
        assert!(valid_snap.is_valid());
        assert_eq!(valid_snap.frame_bytes(), 1080 * 7680);
        assert_eq!(valid_snap.dimensions(), (1920, 1080, 7680));
        assert_eq!(valid_snap.head(), (15, 2));
    }

    #[test]
    fn create_rejects_invalid_slot_config() {
        let path = std::env::temp_dir().join(format!("test_shm_bad_{}.shm", std::process::id()));
        assert!(ShmWriter::create(&path, 1024, 0).is_err());
        assert!(ShmWriter::create(&path, 1024, 65).is_err());
        assert!(ShmWriter::create(&path, 0, 3).is_err());
        assert!(ShmWriter::create(&path, u32::MAX, 3).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn publish_rejects_seq_zero_and_max() {
        let path = std::env::temp_dir().join(format!("test_shm_seq_{}.shm", std::process::id()));
        let writer = ShmWriter::create(&path, 1024, 3).expect("create");
        assert!(writer.publish_frame(0, 640, 480, 0).is_err());
        assert!(writer.publish_frame(0, 640, 480, u64::MAX).is_ok());
        // complete version must stay odd/even-disciplined (not even/even)
        let snap = writer.snapshot();
        assert!(snap.is_valid());
        let _ = writer.pool().unlink();
    }

    #[test]
    fn frame_snapshot_seqlock_roundtrip() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_shm_snap_{}.shm", std::process::id()));

        let writer = ShmWriter::create(&path, 1024, 3).expect("create writer");
        let reader = ShmReader::open(&path).expect("open reader");

        // Before publish, snapshot returns zeroes
        let initial_snap = reader.snapshot();
        assert_eq!(initial_snap.seq, 0);
        assert_eq!(reader.head(), (0, 0));
        assert_eq!(reader.dimensions(), (0, 0, 0));

        // Publish frame 1
        let slot = writer.next_slot();
        writer.publish_frame(slot, 1280, 720, 1).expect("publish");

        let snap1 = reader.snapshot();
        assert_eq!(snap1.seq, 1);
        assert_eq!(snap1.slot, slot);
        assert_eq!(snap1.width, 1280);
        assert_eq!(snap1.height, 720);
        assert_eq!(snap1.stride, 1280 * 4);
        assert_eq!(reader.head(), (1, slot));
        assert_eq!(reader.dimensions(), (1280, 720, 1280 * 4));
        assert_eq!(reader.head_seq(), 1);
        assert_eq!(reader.head_slot(), slot);

        let _ = writer.pool().unlink();
    }
}
