//! In-memory cache of database pages.
//!
//! The buffer pool owns a fixed number of *frames*, each able to hold one
//! page. Callers fetch pages by id and receive a [`PageHandle`], which pins
//! the page in memory until it is dropped. Page contents are accessed
//! through read or write guards on the handle.
//!
//! # Sharding
//!
//! The pool is split into independent shards, each with its own frames,
//! page table, lock, and CLOCK hand. A page always lives in the shard its id
//! hashes to. With one global lock, concurrent cached reads got *slower* as
//! threads were added (see `docs/benchmarks.md`); shards let threads that
//! touch different pages take different locks. Small pools use one shard.
//!
//! # Locking
//!
//! Everything below applies within a single shard; no operation holds
//! locks in two shards at once.
//!
//! - `state` (a per-shard mutex) guards the page table, pin counts, and free
//!   list. It is held only for bookkeeping, and currently also for the disk
//!   read on a cache miss.
//! - Each frame's contents are guarded by their own `RwLock`, so readers of
//!   different pages never block each other once the pages are cached.
//!
//! Lock order is `state` then a frame lock. A thread holding `state` may only
//! block on the lock of an *unpinned* frame: no handle exists for such a
//! frame, so no guard can be holding its lock. Blocking on a pinned frame
//! would deadlock with a thread that holds that frame's guard and is waiting
//! for `state` inside `fetch_page`.
//!
//! Holding `state` during miss I/O serializes misses. It keeps the
//! implementation simple and easy to reason about; lifting it requires an
//! "I/O in progress" frame state and should be justified by a benchmark.
//!
//! # Replacement
//!
//! When no frame is free, a victim is chosen with the CLOCK algorithm: each
//! frame has a reference bit set on access, and the clock hand clears set
//! bits and evicts the first unpinned frame whose bit is already clear.
//! Dirty victims are written back before reuse.
//!
//! Once the WAL exists, write-back must first ensure the log is durable up
//! to the page's LSN. Until then, pages are written back unconditionally.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::{Error, Result};
use crate::storage::disk::DiskManager;
use crate::storage::page::{Page, PageId};

type FrameId = usize;

/// Hasher for the page table. The standard library's default hasher resists
/// hash-flooding attacks from untrusted keys, which costs time on every
/// lookup. Page ids are chosen by the engine, not by users, so a single
/// multiply is enough: it spreads sequential ids over the high bits the
/// table uses for probing, and keeps them distinct in the low bits it uses
/// for bucket selection.
#[derive(Default)]
struct PageIdHasher(u64);

impl Hasher for PageIdHasher {
    fn write_u64(&mut self, value: u64) {
        self.0 = value.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn write(&mut self, bytes: &[u8]) {
        // Only reached if `PageId`'s `Hash` impl changes; stay correct.
        for &byte in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type PageTable = HashMap<PageId, FrameId, BuildHasherDefault<PageIdHasher>>;

/// A cached page and whether it differs from its on-disk copy.
#[derive(Debug)]
struct Frame {
    page: Page,
    dirty: bool,
}

/// Bookkeeping guarded by [`Shard::state`].
#[derive(Debug)]
struct PoolState {
    page_table: PageTable,
    /// Page held by each frame, if any.
    frame_pages: Vec<Option<PageId>>,
    pin_counts: Vec<u32>,
    /// CLOCK reference bits, set whenever a frame is accessed.
    ref_bits: Vec<bool>,
    clock_hand: FrameId,
    free_frames: Vec<FrameId>,
}

/// A fixed-capacity page cache in front of a [`DiskManager`].
#[derive(Debug)]
pub struct BufferPool {
    shards: Vec<Shard>,
}

/// Frames per shard the pool aims for. Large enough that one shard rarely
/// runs out of unpinned frames, small enough to give big pools many shards.
const TARGET_SHARD_FRAMES: usize = 64;

/// Upper bound on shard count; beyond this, contention is no longer the
/// bottleneck and the per-shard overhead only grows.
const MAX_SHARDS: usize = 64;

impl BufferPool {
    /// Creates a pool with room for `capacity` pages.
    pub fn new(disk: Arc<DiskManager>, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::InvalidArgument(
                "buffer pool capacity must be at least 1".into(),
            ));
        }
        let shard_count = shard_count_for(capacity);
        let shards = (0..shard_count)
            .map(|i| {
                // Spread the remainder over the first shards.
                let frames = capacity / shard_count + usize::from(i < capacity % shard_count);
                Shard::new(Arc::clone(&disk), frames)
            })
            .collect();
        Ok(BufferPool { shards })
    }

    /// Number of frames in the pool.
    pub fn capacity(&self) -> usize {
        self.shards.iter().map(|shard| shard.frames.len()).sum()
    }

    /// Returns a pinned handle to page `id`, reading it from disk if needed.
    pub fn fetch_page(&self, id: PageId) -> Result<PageHandle<'_>> {
        self.shard_for(id).fetch_page(id)
    }

    /// Replaces the cached contents of page `id` with `page` and marks it
    /// dirty, without reading the old contents from disk.
    ///
    /// Used to publish pages written by a committed transaction. The page
    /// may be beyond the end of the file; it is written there on write-back.
    pub fn install_page(&self, id: PageId, page: Page) -> Result<()> {
        if page.page_id() != id {
            return Err(Error::InvalidArgument(format!(
                "cannot install {} as {id}",
                page.page_id()
            )));
        }
        self.shard_for(id).install_page(id, page)
    }

    /// Writes page `id` to disk if it is cached and dirty. Does not fsync.
    pub fn flush_page(&self, id: PageId) -> Result<()> {
        self.shard_for(id).flush_page(id)
    }

    /// Writes every dirty cached page to disk and fsyncs the file.
    pub fn flush_all(&self) -> Result<()> {
        for shard in &self.shards {
            shard.flush_cached()?;
        }
        match self.shards.first() {
            Some(shard) => shard.disk.sync(),
            None => Ok(()),
        }
    }

    fn shard_for(&self, id: PageId) -> &Shard {
        // Shard count is a power of two; take high bits of a multiplicative
        // hash so sequential ids spread across shards.
        let hash = id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
        &self.shards[hash as usize & (self.shards.len() - 1)]
    }
}

/// Largest power of two no greater than `capacity / TARGET_SHARD_FRAMES`,
/// clamped to `1..=MAX_SHARDS`.
fn shard_count_for(capacity: usize) -> usize {
    let wanted = (capacity / TARGET_SHARD_FRAMES).clamp(1, MAX_SHARDS);
    1 << wanted.ilog2()
}

/// One independently locked part of the pool.
#[derive(Debug)]
struct Shard {
    disk: Arc<DiskManager>,
    frames: Vec<RwLock<Frame>>,
    state: Mutex<PoolState>,
}

impl Shard {
    fn new(disk: Arc<DiskManager>, capacity: usize) -> Self {
        let frames = (0..capacity)
            .map(|_| {
                RwLock::new(Frame {
                    page: Page::zeroed(),
                    dirty: false,
                })
            })
            .collect();
        let state = PoolState {
            page_table: PageTable::with_capacity_and_hasher(capacity, Default::default()),
            frame_pages: vec![None; capacity],
            pin_counts: vec![0; capacity],
            ref_bits: vec![false; capacity],
            clock_hand: 0,
            // Reversed so frames are handed out in ascending order.
            free_frames: (0..capacity).rev().collect(),
        };
        Shard {
            disk,
            frames,
            state: Mutex::new(state),
        }
    }

    fn fetch_page(&self, id: PageId) -> Result<PageHandle<'_>> {
        let mut state = self.lock_state();
        if let Some(&frame_id) = state.page_table.get(&id) {
            state.pin_counts[frame_id] += 1;
            state.ref_bits[frame_id] = true;
            return Ok(PageHandle {
                pool: self,
                frame_id,
                page_id: id,
            });
        }

        let frame_id = self.acquire_frame(&mut state)?;
        let page = match self.disk.read_page(id) {
            Ok(page) => page,
            Err(err) => {
                state.free_frames.push(frame_id);
                return Err(err);
            }
        };
        // The frame is unpinned and unmapped, so no guard can hold its lock.
        *self.write_frame(frame_id) = Frame { page, dirty: false };
        state.page_table.insert(id, frame_id);
        state.frame_pages[frame_id] = Some(id);
        state.pin_counts[frame_id] = 1;
        state.ref_bits[frame_id] = true;
        Ok(PageHandle {
            pool: self,
            frame_id,
            page_id: id,
        })
    }

    fn install_page(&self, id: PageId, page: Page) -> Result<()> {
        let mut state = self.lock_state();
        if let Some(&frame_id) = state.page_table.get(&id) {
            // Cached and possibly pinned: pin it, then release `state` before
            // waiting for the frame lock (see the module docs on lock order).
            state.pin_counts[frame_id] += 1;
            state.ref_bits[frame_id] = true;
            drop(state);
            let _handle = PageHandle {
                pool: self,
                frame_id,
                page_id: id,
            };
            *self.write_frame(frame_id) = Frame { page, dirty: true };
            return Ok(());
        }
        let frame_id = self.acquire_frame(&mut state)?;
        // The frame is unpinned and unmapped, so no guard can hold its lock.
        *self.write_frame(frame_id) = Frame { page, dirty: true };
        state.page_table.insert(id, frame_id);
        state.frame_pages[frame_id] = Some(id);
        state.ref_bits[frame_id] = true;
        Ok(())
    }

    fn flush_page(&self, id: PageId) -> Result<()> {
        // Pin the frame so it cannot be evicted, then release `state` before
        // taking the frame lock (see the module docs on lock order).
        let handle = {
            let mut state = self.lock_state();
            let Some(&frame_id) = state.page_table.get(&id) else {
                return Ok(());
            };
            state.pin_counts[frame_id] += 1;
            PageHandle {
                pool: self,
                frame_id,
                page_id: id,
            }
        };
        self.write_back(handle.frame_id, id)
    }

    /// Writes every dirty page cached in this shard. Does not fsync.
    fn flush_cached(&self) -> Result<()> {
        let cached: Vec<PageId> = self.lock_state().page_table.keys().copied().collect();
        for id in cached {
            self.flush_page(id)?;
        }
        Ok(())
    }

    /// Returns an empty, unpinned frame, evicting a page if necessary.
    fn acquire_frame(&self, state: &mut PoolState) -> Result<FrameId> {
        if let Some(frame_id) = state.free_frames.pop() {
            return Ok(frame_id);
        }
        let victim = Self::find_victim(state).ok_or_else(|| {
            Error::ResourceExhausted(format!(
                "all {} frames in a buffer pool shard are pinned",
                self.frames.len()
            ))
        })?;
        let old_id = state.frame_pages[victim].expect("occupied frame has a page id");
        // The victim is unpinned, so taking its lock under `state` is safe.
        // On failure the page stays cached and dirty; nothing is lost.
        self.write_back(victim, old_id)?;
        state.page_table.remove(&old_id);
        state.frame_pages[victim] = None;
        Ok(victim)
    }

    /// Picks an unpinned frame using the CLOCK algorithm.
    fn find_victim(state: &mut PoolState) -> Option<FrameId> {
        let capacity = state.frame_pages.len();
        // Two sweeps suffice: the first clears every reference bit, so the
        // second finds any unpinned frame.
        for _ in 0..2 * capacity {
            let frame_id = state.clock_hand;
            state.clock_hand = (state.clock_hand + 1) % capacity;
            if state.pin_counts[frame_id] > 0 {
                continue;
            }
            if state.ref_bits[frame_id] {
                state.ref_bits[frame_id] = false;
                continue;
            }
            return Some(frame_id);
        }
        None
    }

    /// Writes the frame's page to disk if dirty and clears the dirty flag.
    fn write_back(&self, frame_id: FrameId, id: PageId) -> Result<()> {
        let mut frame = self.write_frame(frame_id);
        if frame.dirty {
            self.disk.write_page(id, &mut frame.page)?;
            frame.dirty = false;
        }
        Ok(())
    }

    fn unpin(&self, frame_id: FrameId) {
        let mut state = self.lock_state();
        debug_assert!(
            state.pin_counts[frame_id] > 0,
            "unpinning an unpinned frame"
        );
        state.pin_counts[frame_id] -= 1;
    }

    fn lock_state(&self) -> MutexGuard<'_, PoolState> {
        // Every mutation of `PoolState` leaves it consistent before any call
        // that could panic, so a poisoned lock still holds valid state.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn read_frame(&self, frame_id: FrameId) -> RwLockReadGuard<'_, Frame> {
        self.frames[frame_id]
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_frame(&self, frame_id: FrameId) -> RwLockWriteGuard<'_, Frame> {
        self.frames[frame_id]
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A page pinned in the buffer pool. The page cannot be evicted while any
/// handle to it exists.
#[derive(Debug)]
pub struct PageHandle<'a> {
    pool: &'a Shard,
    frame_id: FrameId,
    page_id: PageId,
}

impl<'a> PageHandle<'a> {
    /// The id of the pinned page.
    pub fn page_id(&self) -> PageId {
        self.page_id
    }

    /// Locks the page for shared reading.
    pub fn read(&self) -> PageReadGuard<'_> {
        PageReadGuard {
            frame: self.pool.read_frame(self.frame_id),
        }
    }

    /// Locks the page for exclusive writing and marks it dirty.
    pub fn write(&self) -> PageWriteGuard<'_> {
        let mut frame = self.pool.write_frame(self.frame_id);
        frame.dirty = true;
        PageWriteGuard { frame }
    }
}

impl Drop for PageHandle<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame_id);
    }
}

/// Shared access to a pinned page.
#[derive(Debug)]
pub struct PageReadGuard<'a> {
    frame: RwLockReadGuard<'a, Frame>,
}

impl Deref for PageReadGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.frame.page
    }
}

/// Exclusive access to a pinned page.
#[derive(Debug)]
pub struct PageWriteGuard<'a> {
    frame: RwLockWriteGuard<'a, Frame>,
}

impl Deref for PageWriteGuard<'_> {
    type Target = Page;

    fn deref(&self) -> &Page {
        &self.frame.page
    }
}

impl DerefMut for PageWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Page {
        &mut self.frame.page
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::PageType;
    use crate::test_util::TempDir;

    /// Creates a database with `pages` heap pages whose payloads are filled
    /// with their page number.
    fn setup(pages: u64) -> (TempDir, Arc<DiskManager>) {
        let dir = TempDir::new();
        let disk = DiskManager::create(&dir.path().join("db.oxen")).unwrap();
        disk.set_page_count(1 + pages).unwrap();
        for id in (1..=pages).map(PageId) {
            let mut page = Page::new(id, PageType::Heap);
            page.payload_mut().fill(id.0 as u8);
            disk.write_page(id, &mut page).unwrap();
        }
        disk.write_file_header().unwrap();
        (dir, Arc::new(disk))
    }

    #[test]
    fn rejects_zero_capacity() {
        let (_dir, disk) = setup(0);
        assert!(matches!(
            BufferPool::new(disk, 0),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn fetch_reads_page_from_disk() {
        let (_dir, disk) = setup(2);
        let pool = BufferPool::new(disk, 4).unwrap();
        let handle = pool.fetch_page(PageId(2)).unwrap();
        assert_eq!(handle.page_id(), PageId(2));
        assert!(handle.read().payload().iter().all(|&b| b == 2));
    }

    #[test]
    fn repeated_fetch_shares_one_frame() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let a = pool.fetch_page(PageId(1)).unwrap();
        let b = pool.fetch_page(PageId(1)).unwrap();
        assert_eq!(a.frame_id, b.frame_id);
        a.write().payload_mut()[0] = 99;
        assert_eq!(b.read().payload()[0], 99);
        assert_eq!(a.pool.lock_state().pin_counts[a.frame_id], 2);
    }

    #[test]
    fn dropping_handles_unpins() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let handle = pool.fetch_page(PageId(1)).unwrap();
        let frame_id = handle.frame_id;
        let second = pool.fetch_page(PageId(1)).unwrap();
        drop(handle);
        assert_eq!(
            pool.shard_for(PageId(1)).lock_state().pin_counts[frame_id],
            1
        );
        drop(second);
        assert_eq!(
            pool.shard_for(PageId(1)).lock_state().pin_counts[frame_id],
            0
        );
    }

    #[test]
    fn write_guard_marks_frame_dirty() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let handle = pool.fetch_page(PageId(1)).unwrap();
        assert!(!handle.pool.read_frame(handle.frame_id).dirty);
        drop(handle.write());
        assert!(handle.pool.read_frame(handle.frame_id).dirty);
    }

    impl BufferPool {
        fn cached_page_count(&self) -> usize {
            self.shards
                .iter()
                .map(|shard| shard.lock_state().page_table.len())
                .sum()
        }
    }

    fn reopen_disk(dir: &TempDir) -> Arc<DiskManager> {
        Arc::new(DiskManager::open(&dir.path().join("db.oxen")).unwrap())
    }

    #[test]
    fn evicts_unpinned_pages_when_full() {
        let (_dir, disk) = setup(5);
        let pool = BufferPool::new(disk, 2).unwrap();
        for id in 1..=5 {
            let handle = pool.fetch_page(PageId(id)).unwrap();
            assert!(handle.read().payload().iter().all(|&b| b == id as u8));
        }
        assert_eq!(pool.cached_page_count(), 2);
    }

    #[test]
    fn dirty_pages_survive_eviction() {
        let (_dir, disk) = setup(3);
        let pool = BufferPool::new(disk, 1).unwrap();
        pool.fetch_page(PageId(1)).unwrap().write().payload_mut()[0] = 200;
        pool.fetch_page(PageId(2)).unwrap();
        pool.fetch_page(PageId(3)).unwrap();
        assert_eq!(pool.fetch_page(PageId(1)).unwrap().read().payload()[0], 200);
    }

    #[test]
    fn pinned_pages_are_not_evicted() {
        let (_dir, disk) = setup(3);
        let pool = BufferPool::new(disk, 2).unwrap();
        let pinned = pool.fetch_page(PageId(1)).unwrap();
        pinned.write().payload_mut()[0] = 42;
        for id in [2, 3, 2, 3] {
            pool.fetch_page(PageId(id)).unwrap();
        }
        assert_eq!(pinned.read().payload()[0], 42);
        assert_eq!(
            pool.shard_for(PageId(1))
                .lock_state()
                .page_table
                .get(&PageId(1)),
            Some(&pinned.frame_id)
        );
    }

    #[test]
    fn flush_page_writes_to_disk() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(Arc::clone(&disk), 4).unwrap();
        let handle = pool.fetch_page(PageId(1)).unwrap();
        handle.write().payload_mut()[0] = 123;
        pool.flush_page(PageId(1)).unwrap();
        assert!(!handle.pool.read_frame(handle.frame_id).dirty);
        assert_eq!(disk.read_page(PageId(1)).unwrap().payload()[0], 123);
    }

    #[test]
    fn flush_page_of_uncached_page_is_a_no_op() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        pool.flush_page(PageId(1)).unwrap();
    }

    #[test]
    fn flush_all_persists_across_reopen() {
        let (dir, disk) = setup(3);
        {
            let pool = BufferPool::new(disk, 4).unwrap();
            for id in 1..=3 {
                pool.fetch_page(PageId(id)).unwrap().write().payload_mut()[0] = 100 + id as u8;
            }
            pool.flush_all().unwrap();
        }
        let pool = BufferPool::new(reopen_disk(&dir), 4).unwrap();
        for id in 1..=3 {
            assert_eq!(
                pool.fetch_page(PageId(id)).unwrap().read().payload()[0],
                100 + id as u8
            );
        }
    }

    #[test]
    fn concurrent_increments_are_not_lost() {
        const THREADS: u64 = 8;
        const PAGES: u64 = 20;
        const INCREMENTS: u64 = 2_000;

        let (_dir, disk) = setup(PAGES);
        // Fewer frames than pages forces constant eviction and write-back,
        // but enough that every thread can hold one pin at a time.
        let pool = BufferPool::new(disk, THREADS as usize + 2).unwrap();
        for id in 1..=PAGES {
            pool.fetch_page(PageId(id)).unwrap().write().payload_mut()[..8].fill(0);
        }

        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let pool = &pool;
                scope.spawn(move || {
                    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ thread;
                    for _ in 0..INCREMENTS {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        let handle = pool.fetch_page(PageId(1 + rng % PAGES)).unwrap();
                        let mut page = handle.write();
                        let counter = &mut page.payload_mut()[..8];
                        let value = u64::from_le_bytes(counter.try_into().unwrap()) + 1;
                        counter.copy_from_slice(&value.to_le_bytes());
                    }
                });
            }
        });

        let total: u64 = (1..=PAGES)
            .map(|id| {
                let handle = pool.fetch_page(PageId(id)).unwrap();
                let page = handle.read();
                u64::from_le_bytes(page.payload()[..8].try_into().unwrap())
            })
            .sum();
        assert_eq!(total, THREADS * INCREMENTS);
        assert!(
            pool.shards.iter().all(|shard| shard
                .lock_state()
                .pin_counts
                .iter()
                .all(|&pins| pins == 0))
        );
    }

    #[test]
    fn install_replaces_cached_page() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let handle = pool.fetch_page(PageId(1)).unwrap();
        let mut replacement = Page::new(PageId(1), PageType::Heap);
        replacement.payload_mut()[0] = 55;
        pool.install_page(PageId(1), replacement).unwrap();
        assert_eq!(handle.read().payload()[0], 55);
        assert!(handle.pool.read_frame(handle.frame_id).dirty);
    }

    #[test]
    fn install_uncached_page_does_not_read_disk() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        // Page 1 on disk is valid; install a different version without
        // fetching and check the installed one wins.
        let mut replacement = Page::new(PageId(1), PageType::Heap);
        replacement.payload_mut()[0] = 66;
        pool.install_page(PageId(1), replacement).unwrap();
        assert_eq!(pool.fetch_page(PageId(1)).unwrap().read().payload()[0], 66);
    }

    #[test]
    fn installed_page_is_unpinned_and_evictable() {
        let (_dir, disk) = setup(2);
        let pool = BufferPool::new(Arc::clone(&disk), 1).unwrap();
        let mut replacement = Page::new(PageId(1), PageType::Heap);
        replacement.payload_mut()[0] = 77;
        pool.install_page(PageId(1), replacement).unwrap();
        // Fetching another page evicts the installed one, writing it back.
        pool.fetch_page(PageId(2)).unwrap();
        assert_eq!(disk.read_page(PageId(1)).unwrap().payload()[0], 77);
    }

    #[test]
    fn install_rejects_mismatched_page_id() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let page = Page::new(PageId(2), PageType::Heap);
        assert!(matches!(
            pool.install_page(PageId(1), page),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn page_id_hasher_spreads_sequential_ids() {
        use std::collections::HashSet;
        use std::hash::BuildHasher;
        let build = BuildHasherDefault::<PageIdHasher>::default();
        let hashes: Vec<u64> = (0..4096u64).map(|id| build.hash_one(PageId(id))).collect();
        // Distinct in the low 12 bits (bucket index for a 4096-slot table)...
        let low: HashSet<u64> = hashes.iter().map(|h| h & 0xFFF).collect();
        assert_eq!(low.len(), 4096);
        // ...and using most of the top 7 bits (the probe tag).
        let tags: HashSet<u64> = hashes.iter().map(|h| h >> 57).collect();
        assert!(tags.len() > 120, "only {} distinct tags", tags.len());
    }

    #[test]
    fn shard_count_scales_with_capacity() {
        assert_eq!(shard_count_for(1), 1);
        assert_eq!(shard_count_for(127), 1);
        assert_eq!(shard_count_for(128), 2);
        assert_eq!(shard_count_for(1000), 8);
        assert_eq!(shard_count_for(4096), 64);
        assert_eq!(shard_count_for(1 << 20), 64);
    }

    #[test]
    fn capacity_is_split_exactly_across_shards() {
        let (_dir, disk) = setup(0);
        for capacity in [1, 63, 128, 1000, 4097] {
            let pool = BufferPool::new(Arc::clone(&disk), capacity).unwrap();
            assert_eq!(pool.capacity(), capacity);
            assert!(pool.shards.iter().all(|shard| !shard.frames.is_empty()));
        }
    }

    #[test]
    fn pages_spread_across_shards() {
        let (_dir, disk) = setup(512);
        let pool = BufferPool::new(disk, 1024).unwrap(); // 16 shards
        for id in 1..=512 {
            pool.fetch_page(PageId(id)).unwrap();
        }
        let per_shard: Vec<usize> = pool
            .shards
            .iter()
            .map(|shard| shard.lock_state().page_table.len())
            .collect();
        // 32 per shard on average; no shard should be empty or hold most.
        assert!(
            per_shard.iter().all(|&n| (8..=64).contains(&n)),
            "{per_shard:?}"
        );
    }

    #[test]
    fn concurrent_increments_across_shards_are_not_lost() {
        const THREADS: u64 = 8;
        const PAGES: u64 = 2_000;
        const INCREMENTS: u64 = 5_000;

        let (_dir, disk) = setup(PAGES);
        // 4 shards of 64 frames against 2,000 pages: constant eviction in
        // every shard.
        let pool = BufferPool::new(disk, 256).unwrap();
        assert_eq!(pool.shards.len(), 4);
        for id in 1..=PAGES {
            pool.fetch_page(PageId(id)).unwrap().write().payload_mut()[..8].fill(0);
        }

        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let pool = &pool;
                scope.spawn(move || {
                    let mut rng = 0x2545_F491_4F6C_DD1Du64 ^ (thread + 1);
                    for _ in 0..INCREMENTS {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        let handle = pool.fetch_page(PageId(1 + rng % PAGES)).unwrap();
                        let mut page = handle.write();
                        let counter = &mut page.payload_mut()[..8];
                        let value = u64::from_le_bytes(counter.try_into().unwrap()) + 1;
                        counter.copy_from_slice(&value.to_le_bytes());
                    }
                });
            }
        });

        let total: u64 = (1..=PAGES)
            .map(|id| {
                let handle = pool.fetch_page(PageId(id)).unwrap();
                let page = handle.read();
                u64::from_le_bytes(page.payload()[..8].try_into().unwrap())
            })
            .sum();
        assert_eq!(total, THREADS * INCREMENTS);
    }

    #[test]
    fn full_pool_reports_exhaustion() {
        let (_dir, disk) = setup(2);
        let pool = BufferPool::new(disk, 1).unwrap();
        let _held = pool.fetch_page(PageId(1)).unwrap();
        assert!(matches!(
            pool.fetch_page(PageId(2)),
            Err(Error::ResourceExhausted(_))
        ));
    }

    #[test]
    fn failed_read_returns_frame_to_free_list() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 1).unwrap();
        assert!(pool.fetch_page(PageId(50)).is_err());
        assert!(pool.fetch_page(PageId(1)).is_ok());
    }
}
