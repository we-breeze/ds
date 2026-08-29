use std::cell::UnsafeCell;
use std::fmt::{self, Debug, Formatter};
use std::io;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::ptr;
use std::slice;
use std::sync::Arc;

#[cfg(all(loom, test))]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
#[cfg(not(all(loom, test)))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

const CHUNK_COUNT: usize = 2;
/// Frames up to this size are stored directly in the frame value.
pub const EPHEMERAL_BYTES_INLINE_CAPACITY: usize = 48;
const FROZEN: u64 = 1 << 63;
const OFFSET_MASK: u64 = FROZEN - 1;
const ALLOCATED_ONE: u64 = 1 << 32;

/// A shared allocator for short-lived encoded byte frames.
///
/// Frames of [`EPHEMERAL_BYTES_INLINE_CAPACITY`] bytes or less stay inline. For
/// larger frames, the arena owns two fixed-size bump-allocation chunks. An
/// allocation that no longer fits freezes the current chunk and switches to the
/// other one. A frozen chunk is reset as a whole after all allocations issued
/// from it have been dropped. Allocation never waits: oversized frames, or
/// frames for which neither chunk is available, fall back to a private heap
/// buffer.
#[derive(Clone)]
pub struct EphemeralBytesArena {
    inner: Arc<ArenaInner>,
}

impl EphemeralBytesArena {
    /// Creates an arena containing two chunks of `chunk_capacity` bytes each.
    pub fn new(chunk_capacity: usize) -> Self {
        assert!(chunk_capacity > 0, "chunk capacity must be positive");
        assert!(
            chunk_capacity < u32::MAX as usize,
            "chunk capacity must be less than 4 GiB"
        );
        assert!(
            chunk_capacity as u128 <= OFFSET_MASK as u128,
            "chunk capacity exceeds the cursor representation"
        );

        Self {
            inner: Arc::new(ArenaInner {
                current: AtomicUsize::new(0),
                chunks: std::array::from_fn(|_| Chunk::new(chunk_capacity)),
            }),
        }
    }

    /// Reserves space for one frame.
    ///
    /// The returned buffer starts empty and can grow up to `capacity` without
    /// another allocation. Small frames stay inline; dropping an arena-backed
    /// frame before or after [`EphemeralBytesMut::freeze`] returns its chunk
    /// allocation automatically.
    #[inline]
    pub fn alloc(&self, capacity: usize) -> EphemeralBytesMut {
        let storage = if capacity <= EPHEMERAL_BYTES_INLINE_CAPACITY {
            Storage::Inline([MaybeUninit::uninit(); EPHEMERAL_BYTES_INLINE_CAPACITY])
        } else if capacity > self.chunk_capacity() {
            Storage::Heap(Vec::with_capacity(capacity))
        } else if let Some(allocation) = self.inner.reserve(capacity) {
            Storage::Arena(allocation)
        } else {
            Storage::Heap(Vec::with_capacity(capacity))
        };

        EphemeralBytesMut {
            storage: Some(storage),
            len: 0,
        }
    }

    /// Copies a slice directly into its final storage and freezes it.
    #[inline]
    pub fn copy_from_slice(&self, bytes: &[u8]) -> EphemeralBytes {
        let mut output = self.alloc(bytes.len());
        output.extend_from_slice(bytes);
        output.freeze()
    }

    /// Returns the capacity of each of the two chunks.
    #[inline]
    pub fn chunk_capacity(&self) -> usize {
        self.inner.chunks[0].state.capacity
    }
}

impl Debug for EphemeralBytesArena {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EphemeralBytesArena")
            .field("chunk_capacity", &self.chunk_capacity())
            .finish_non_exhaustive()
    }
}

struct ArenaInner {
    current: AtomicUsize,
    chunks: [Chunk; CHUNK_COUNT],
}

impl ArenaInner {
    #[inline]
    fn reserve(self: &Arc<Self>, len: usize) -> Option<ArenaAllocation> {
        let first = self.current.load(Acquire) & 1;
        if let Some(offset) = self.chunks[first].reserve(len) {
            return Some(ArenaAllocation::new(Arc::clone(self), first, offset, len));
        }

        let second = 1 - first;
        let _ = self
            .current
            .compare_exchange(first, second, AcqRel, Acquire);
        self.chunks[second]
            .reserve(len)
            .map(|offset| ArenaAllocation::new(Arc::clone(self), second, offset, len))
    }
}

#[repr(align(64))]
struct Chunk {
    data: Box<[UnsafeCell<u8>]>,
    state: ChunkState,
}

struct ChunkState {
    capacity: usize,
    /// High bit: frozen; remaining bits: next byte offset.
    cursor: AtomicU64,
    /// High 32 bits: allocated tickets; low 32 bits: released/cancelled tickets.
    allocated_released: AtomicU64,
}

impl Chunk {
    fn new(capacity: usize) -> Self {
        let data = (0..capacity)
            .map(|_| UnsafeCell::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            data,
            state: ChunkState::new(capacity),
        }
    }

    #[inline]
    fn reserve(&self, len: usize) -> Option<usize> {
        self.state.reserve(len)
    }

    #[inline]
    fn data_ptr(&self) -> *mut u8 {
        self.data.as_ptr().cast::<u8>().cast_mut()
    }
}

impl ChunkState {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            cursor: AtomicU64::new(0),
            allocated_released: AtomicU64::new(0),
        }
    }

    #[inline]
    fn reserve(&self, len: usize) -> Option<usize> {
        debug_assert!(len > 0 && len <= self.capacity);

        // Avoid touching the ticket counter for the common already-frozen case.
        // A concurrent freeze is covered by the second cursor check below.
        if self.cursor.load(Acquire) & FROZEN != 0 {
            return None;
        }

        self.allocate_ticket();
        let mut cursor = self.cursor.load(Acquire);
        loop {
            if cursor & FROZEN != 0 {
                self.release_ticket();
                return None;
            }

            let offset = cursor & OFFSET_MASK;
            let Some(end) = offset.checked_add(len as u64) else {
                if self.freeze_or_retry(cursor, &mut cursor) {
                    self.release_ticket();
                    return None;
                }
                continue;
            };

            if end > self.capacity as u64 {
                if self.freeze_or_retry(cursor, &mut cursor) {
                    self.release_ticket();
                    return None;
                }
                continue;
            }

            match self
                .cursor
                .compare_exchange_weak(cursor, end, AcqRel, Acquire)
            {
                Ok(_) => return Some(offset as usize),
                Err(actual) => cursor = actual,
            }
        }
    }

    #[inline]
    fn freeze_or_retry(&self, cursor: u64, actual: &mut u64) -> bool {
        match self
            .cursor
            .compare_exchange(cursor, cursor | FROZEN, AcqRel, Acquire)
        {
            Ok(_) => true,
            Err(value) => {
                *actual = value;
                false
            }
        }
    }

    #[inline]
    fn allocate_ticket(&self) {
        let old = self.allocated_released.fetch_add(ALLOCATED_ONE, AcqRel);
        debug_assert_ne!(old >> 32, u32::MAX as u64, "allocation counter overflow");
    }

    #[inline]
    fn release_ticket(&self) {
        let old = self.allocated_released.fetch_add(1, AcqRel);
        let allocated = (old >> 32) as u32;
        let released = (old as u32)
            .checked_add(1)
            .expect("release counter overflow");
        assert!(
            released <= allocated,
            "released ticket count exceeds allocated ticket count"
        );

        if released == allocated {
            self.try_reset((u64::from(allocated) << 32) | u64::from(released));
        }
    }

    #[inline]
    fn try_reset(&self, balanced_counts: u64) {
        let frozen_cursor = self.cursor.load(Acquire);
        if frozen_cursor & FROZEN == 0 {
            return;
        }

        // The exact CAS protects against a reserve operation which observed the
        // chunk before it was frozen and acquired a ticket concurrently.
        if self
            .allocated_released
            .compare_exchange(balanced_counts, 0, AcqRel, Acquire)
            .is_ok()
        {
            let _ = self
                .cursor
                .compare_exchange(frozen_cursor, 0, Release, Relaxed);
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> (bool, usize, u32, u32) {
        let cursor = self.cursor.load(Acquire);
        let counts = self.allocated_released.load(Acquire);
        (
            cursor & FROZEN != 0,
            (cursor & OFFSET_MASK) as usize,
            (counts >> 32) as u32,
            counts as u32,
        )
    }
}

// SAFETY: the bump cursor gives every live allocation a disjoint byte range.
// A chunk is reset only after every ticket has been released, so raw accesses
// through one allocation never overlap a concurrently live allocation.
unsafe impl Sync for Chunk {}

struct ArenaAllocation {
    arena: Arc<ArenaInner>,
    chunk: usize,
    offset: usize,
    capacity: usize,
}

impl ArenaAllocation {
    fn new(arena: Arc<ArenaInner>, chunk: usize, offset: usize, capacity: usize) -> Self {
        Self {
            arena,
            chunk,
            offset,
            capacity,
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut u8 {
        // SAFETY: `offset` is returned by this chunk's bounded cursor and the
        // allocation owns the following `capacity` bytes until Drop.
        unsafe { self.arena.chunks[self.chunk].data_ptr().add(self.offset) }
    }
}

impl Drop for ArenaAllocation {
    #[inline]
    fn drop(&mut self) {
        self.arena.chunks[self.chunk].state.release_ticket();
    }
}

enum Storage {
    Inline([MaybeUninit<u8>; EPHEMERAL_BYTES_INLINE_CAPACITY]),
    Arena(ArenaAllocation),
    Heap(Vec<u8>),
}

impl Storage {
    #[inline]
    fn capacity(&self) -> usize {
        match self {
            Self::Inline(_) => EPHEMERAL_BYTES_INLINE_CAPACITY,
            Self::Arena(allocation) => allocation.capacity,
            Self::Heap(bytes) => bytes.capacity(),
        }
    }

    #[inline]
    fn is_heap(&self) -> bool {
        matches!(self, Self::Heap(_))
    }

    #[inline]
    fn is_inline(&self) -> bool {
        matches!(self, Self::Inline(_))
    }

    #[inline]
    fn as_slice(&self, len: usize) -> &[u8] {
        match self {
            Self::Inline(bytes) => {
                // SAFETY: only the initialized prefix, tracked by `len`, is
                // exposed. `extend_from_slice` initializes it before `len`
                // advances.
                unsafe { slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), len) }
            }
            Self::Arena(allocation) => {
                // SAFETY: the allocation remains live for the returned borrow,
                // and `len` never exceeds its reserved range.
                unsafe { slice::from_raw_parts(allocation.as_mut_ptr(), len) }
            }
            Self::Heap(bytes) => bytes.as_slice(),
        }
    }

    #[inline]
    fn extend_from_slice(&mut self, offset: usize, bytes: &[u8]) {
        match self {
            Self::Inline(output) => {
                // SAFETY: the caller checked the write against the inline
                // capacity. The written prefix is marked live by advancing
                // the frame length immediately after this call.
                unsafe {
                    ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        output.as_mut_ptr().cast::<u8>().add(offset),
                        bytes.len(),
                    );
                }
            }
            Self::Arena(allocation) => {
                // SAFETY: `offset + bytes.len()` was checked against the unique
                // allocation's capacity by the caller.
                unsafe {
                    ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        allocation.as_mut_ptr().add(offset),
                        bytes.len(),
                    );
                }
            }
            Self::Heap(output) => output.extend_from_slice(bytes),
        }
    }
}

/// A writable, fixed-capacity frame allocated by [`EphemeralBytesArena`].
///
/// Encoding writes directly into its final backing memory. It never grows;
/// reserve the complete frame capacity up front and call [`freeze`](Self::freeze)
/// before placing it on an asynchronous request queue.
pub struct EphemeralBytesMut {
    storage: Option<Storage>,
    len: usize,
}

impl EphemeralBytesMut {
    /// Number of bytes written so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Maximum number of bytes this allocation can hold.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.storage().capacity()
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.capacity() - self.len
    }

    /// Whether this allocation used the heap fallback instead of an arena chunk.
    #[inline]
    pub fn is_heap_allocated(&self) -> bool {
        self.storage().is_heap()
    }

    /// Whether the bytes are stored directly in this frame value.
    #[inline]
    pub fn is_inline(&self) -> bool {
        self.storage().is_inline()
    }

    /// Appends bytes without performing another allocation.
    ///
    /// Panics if the originally reserved capacity is insufficient.
    #[inline]
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        assert!(
            bytes.len() <= self.remaining(),
            "ephemeral byte capacity exceeded: {} > {}",
            bytes.len(),
            self.remaining()
        );
        let offset = self.len;
        self.storage_mut().extend_from_slice(offset, bytes);
        self.len += bytes.len();
    }

    /// Converts the writable frame into immutable bytes without copying.
    #[inline]
    pub fn freeze(mut self) -> EphemeralBytes {
        EphemeralBytes {
            storage: self.storage.take().expect("storage is present"),
            len: self.len,
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.storage().as_slice(self.len)
    }

    #[inline]
    fn storage(&self) -> &Storage {
        self.storage.as_ref().expect("storage is present")
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut Storage {
        self.storage.as_mut().expect("storage is present")
    }
}

impl io::Write for EphemeralBytesMut {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let len = bytes.len().min(self.remaining());
        self.extend_from_slice(&bytes[..len]);
        Ok(len)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsRef<[u8]> for EphemeralBytesMut {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Debug for EphemeralBytesMut {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EphemeralBytesMut")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("heap_allocated", &self.is_heap_allocated())
            .finish()
    }
}

/// Immutable, move-only bytes backed by inline storage, an arena allocation, or
/// a heap fallback.
///
/// Dropping the value releases its allocation ticket. The underlying chunk is
/// reused only after every value issued from the frozen chunk has been dropped.
pub struct EphemeralBytes {
    storage: Storage,
    len: usize,
}

impl EphemeralBytes {
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_heap_allocated(&self) -> bool {
        self.storage.is_heap()
    }

    /// Whether the bytes are stored directly in this frame value.
    #[inline]
    pub fn is_inline(&self) -> bool {
        self.storage.is_inline()
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.storage.as_slice(self.len)
    }
}

impl AsRef<[u8]> for EphemeralBytes {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl Deref for EphemeralBytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl Debug for EphemeralBytes {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EphemeralBytes")
            .field("len", &self.len())
            .field("heap_allocated", &self.is_heap_allocated())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Barrier;
    use std::thread;

    fn chunk_index(bytes: &EphemeralBytesMut) -> Option<usize> {
        match bytes.storage() {
            Storage::Inline(_) => None,
            Storage::Arena(allocation) => Some(allocation.chunk),
            Storage::Heap(_) => None,
        }
    }

    #[test]
    fn small_frames_use_all_48_inline_bytes_without_touching_the_arena() {
        let arena = EphemeralBytesArena::new(64);
        let before = arena.inner.chunks[0].state.snapshot();
        let mut bytes = arena.alloc(EPHEMERAL_BYTES_INLINE_CAPACITY);

        assert!(bytes.is_inline());
        assert_eq!(bytes.capacity(), EPHEMERAL_BYTES_INLINE_CAPACITY);
        bytes.extend_from_slice(&[0x5a; EPHEMERAL_BYTES_INLINE_CAPACITY]);
        let bytes = bytes.freeze();

        assert!(bytes.is_inline());
        assert_eq!(bytes.len(), EPHEMERAL_BYTES_INLINE_CAPACITY);
        assert!(bytes.iter().all(|byte| *byte == 0x5a));
        assert_eq!(arena.inner.chunks[0].state.snapshot(), before);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn inline_storage_has_the_expected_64_bit_layout() {
        assert_eq!(std::mem::size_of::<Storage>(), 56);
        assert_eq!(std::mem::size_of::<EphemeralBytesMut>(), 64);
        assert_eq!(std::mem::size_of::<EphemeralBytes>(), 64);
    }

    #[test]
    fn writes_and_freezes_without_copying_storage() {
        let arena = EphemeralBytesArena::new(64);
        let mut bytes = arena.alloc(64);
        assert_eq!(chunk_index(&bytes), Some(0));

        bytes.extend_from_slice(b"GET ");
        bytes.write_all(b"key\r\n").unwrap();
        let ptr = bytes.as_slice().as_ptr();
        let bytes = bytes.freeze();

        assert_eq!(&*bytes, b"GET key\r\n");
        assert_eq!(bytes.as_ptr(), ptr);
        assert!(!bytes.is_heap_allocated());
    }

    #[test]
    fn switches_chunks_and_falls_back_without_waiting() {
        let arena = EphemeralBytesArena::new(64);
        let first = arena.alloc(60);
        assert_eq!(chunk_index(&first), Some(0));

        let second = arena.alloc(50);
        assert_eq!(chunk_index(&second), Some(1));

        let fallback = arena.alloc(50);
        assert!(fallback.is_heap_allocated());
    }

    #[test]
    fn frozen_chunk_resets_after_out_of_order_release() {
        let arena = EphemeralBytesArena::new(128);
        let first = arena.alloc(60);
        let second = arena.alloc(60);
        let other_chunk = arena.alloc(60);

        assert_eq!(chunk_index(&first), Some(0));
        assert_eq!(chunk_index(&second), Some(0));
        assert_eq!(chunk_index(&other_chunk), Some(1));
        assert_eq!(arena.inner.chunks[0].state.snapshot(), (true, 120, 3, 1));

        drop(second);
        assert_eq!(arena.inner.chunks[0].state.snapshot(), (true, 120, 3, 2));
        drop(first);
        assert_eq!(arena.inner.chunks[0].state.snapshot(), (false, 0, 0, 0));
    }

    #[test]
    fn oversized_frames_use_heap_and_empty_frames_stay_inline() {
        let arena = EphemeralBytesArena::new(64);
        assert!(arena.alloc(65).is_heap_allocated());
        assert!(arena.alloc(0).is_inline());
    }

    #[test]
    fn concurrent_allocations_keep_live_frames_disjoint() {
        const THREADS: usize = 8;
        const ROUNDS: usize = 500;

        let arena = EphemeralBytesArena::new(1 << 12);
        let start = Arc::new(Barrier::new(THREADS));
        thread::scope(|scope| {
            for worker in 0..THREADS {
                let arena = arena.clone();
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    let marker = worker as u8 + 1;
                    let mut live = Vec::with_capacity(16);
                    for round in 0..ROUNDS {
                        let len =
                            EPHEMERAL_BYTES_INLINE_CAPACITY + 1 + (round * 17 + worker * 13) % 48;
                        let mut bytes = arena.alloc(len);
                        bytes.extend_from_slice(&vec![marker; len]);
                        live.push(bytes.freeze());

                        if live.len() == 16 {
                            for bytes in live.drain(..).rev() {
                                assert!(bytes.iter().all(|byte| *byte == marker));
                            }
                        }
                    }
                    for bytes in live.into_iter().rev() {
                        assert!(bytes.iter().all(|byte| *byte == marker));
                    }
                });
            }
        });
    }

    #[test]
    fn frame_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EphemeralBytesArena>();
        assert_send_sync::<EphemeralBytesMut>();
        assert_send_sync::<EphemeralBytes>();
    }

    #[test]
    #[should_panic(expected = "ephemeral byte capacity exceeded")]
    fn cannot_grow_past_reserved_capacity() {
        let arena = EphemeralBytesArena::new(64);
        let mut bytes = arena.alloc(EPHEMERAL_BYTES_INLINE_CAPACITY);
        bytes.extend_from_slice(&[0; EPHEMERAL_BYTES_INLINE_CAPACITY + 1]);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::model::Builder;
    use loom::sync::Arc as LoomArc;
    use loom::thread;

    struct ModelLease {
        state: LoomArc<ChunkState>,
        live_bytes: LoomArc<AtomicUsize>,
        mask: usize,
    }

    impl Drop for ModelLease {
        fn drop(&mut self) {
            // Logical byte ownership is given up before the allocation ticket.
            // Therefore, a subsequent reset may safely reuse the byte range.
            let old = self.live_bytes.fetch_and(!self.mask, AcqRel);
            assert_eq!(
                old & self.mask,
                self.mask,
                "released byte range is not live"
            );
            self.state.release_ticket();
        }
    }

    fn reserve(
        state: &LoomArc<ChunkState>,
        live_bytes: &LoomArc<AtomicUsize>,
        len: usize,
    ) -> Option<ModelLease> {
        let offset = state.reserve(len)?;
        let mask = ((1usize << len) - 1) << offset;
        let old = live_bytes.fetch_or(mask, AcqRel);
        assert_eq!(old & mask, 0, "live byte ranges overlap");
        Some(ModelLease {
            state: LoomArc::clone(state),
            live_bytes: LoomArc::clone(live_bytes),
            mask,
        })
    }

    fn assert_quiescent(state: &ChunkState, live_bytes: &AtomicUsize) {
        let (frozen, offset, allocated, released) = state.snapshot();
        assert_eq!(live_bytes.load(Acquire), 0);
        assert!(!frozen, "balanced frozen chunk was not reset");
        assert!(offset <= state.capacity);
        assert_eq!(allocated, released);
    }

    fn model(check: impl Fn() + Send + Sync + 'static) {
        let mut builder = Builder::new();
        builder.max_branches = 10_000;
        builder.check(check);
    }

    #[test]
    fn loom_concurrent_reservations_never_overlap() {
        model(|| {
            let state = LoomArc::new(ChunkState::new(2));
            let live_bytes = LoomArc::new(AtomicUsize::new(0));

            let first = {
                let state = LoomArc::clone(&state);
                let live_bytes = LoomArc::clone(&live_bytes);
                thread::spawn(move || {
                    let lease = reserve(&state, &live_bytes, 1).expect("first byte fits");
                    thread::yield_now();
                    drop(lease);
                })
            };
            let second = {
                let state = LoomArc::clone(&state);
                let live_bytes = LoomArc::clone(&live_bytes);
                thread::spawn(move || {
                    let lease = reserve(&state, &live_bytes, 1).expect("second byte fits");
                    thread::yield_now();
                    drop(lease);
                })
            };

            first.join().unwrap();
            second.join().unwrap();
            assert_quiescent(&state, &live_bytes);
        });
    }

    #[test]
    fn loom_freeze_racing_last_release_resets_the_chunk() {
        model(|| {
            let state = LoomArc::new(ChunkState::new(2));
            let live_bytes = LoomArc::new(AtomicUsize::new(0));
            let held = reserve(&state, &live_bytes, 1).expect("initial byte fits");

            let freezer = {
                let state = LoomArc::clone(&state);
                let live_bytes = LoomArc::clone(&live_bytes);
                thread::spawn(move || {
                    assert!(reserve(&state, &live_bytes, 2).is_none());
                })
            };
            let releaser = thread::spawn(move || {
                thread::yield_now();
                drop(held);
            });

            freezer.join().unwrap();
            releaser.join().unwrap();
            assert_eq!(state.snapshot(), (false, 0, 0, 0));
            assert_eq!(live_bytes.load(Acquire), 0);
        });
    }

    #[test]
    fn loom_stale_reserver_cannot_escape_ticket_accounting() {
        model(|| {
            let state = LoomArc::new(ChunkState::new(1));
            let live_bytes = LoomArc::new(AtomicUsize::new(0));
            let held = reserve(&state, &live_bytes, 1).expect("initial byte fits");

            let contender = {
                let state = LoomArc::clone(&state);
                let live_bytes = LoomArc::clone(&live_bytes);
                thread::spawn(move || {
                    if let Some(lease) = reserve(&state, &live_bytes, 1) {
                        thread::yield_now();
                        drop(lease);
                    }
                })
            };
            let releaser = {
                let state = LoomArc::clone(&state);
                let live_bytes = LoomArc::clone(&live_bytes);
                thread::spawn(move || {
                    drop(held);
                    if let Some(lease) = reserve(&state, &live_bytes, 1) {
                        thread::yield_now();
                        drop(lease);
                    }
                })
            };

            contender.join().unwrap();
            releaser.join().unwrap();
            assert_quiescent(&state, &live_bytes);
        });
    }

    #[test]
    fn loom_out_of_order_releases_cannot_reset_early() {
        model(|| {
            let state = LoomArc::new(ChunkState::new(2));
            let live_bytes = LoomArc::new(AtomicUsize::new(0));
            let first = reserve(&state, &live_bytes, 1).expect("first byte fits");
            let second = reserve(&state, &live_bytes, 1).expect("second byte fits");

            let release_first = thread::spawn(move || {
                thread::yield_now();
                drop(first);
            });
            let release_second = thread::spawn(move || drop(second));

            // The chunk is full. This request freezes it and races both releases.
            assert!(reserve(&state, &live_bytes, 1).is_none());
            release_second.join().unwrap();
            release_first.join().unwrap();

            assert_eq!(state.snapshot(), (false, 0, 0, 0));
            assert_eq!(live_bytes.load(Acquire), 0);
        });
    }
}
