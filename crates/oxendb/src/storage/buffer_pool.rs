//! In-memory cache of database pages.
//!
//! The buffer pool owns a fixed number of *frames*, each able to hold one
//! page. Callers fetch pages by id and receive a [`PageHandle`], which pins
//! the page in memory until it is dropped. Page contents are accessed
//! through read or write guards on the handle.
//!
//! # Locking
//!
//! - `state` (a single mutex) guards the page table, pin counts, and free
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
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::{Error, Result};
use crate::storage::disk::DiskManager;
use crate::storage::page::{Page, PageId, PageType};

type FrameId = usize;

/// A cached page and whether it differs from its on-disk copy.
#[derive(Debug)]
struct Frame {
    page: Page,
    dirty: bool,
}

/// Bookkeeping guarded by [`BufferPool::state`].
#[derive(Debug)]
struct PoolState {
    page_table: HashMap<PageId, FrameId>,
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
    disk: Arc<DiskManager>,
    frames: Vec<RwLock<Frame>>,
    state: Mutex<PoolState>,
}

impl BufferPool {
    /// Creates a pool with room for `capacity` pages.
    pub fn new(disk: Arc<DiskManager>, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::InvalidArgument(
                "buffer pool capacity must be at least 1".into(),
            ));
        }
        let frames = (0..capacity)
            .map(|_| {
                RwLock::new(Frame {
                    page: Page::zeroed(),
                    dirty: false,
                })
            })
            .collect();
        let state = PoolState {
            page_table: HashMap::with_capacity(capacity),
            frame_pages: vec![None; capacity],
            pin_counts: vec![0; capacity],
            ref_bits: vec![false; capacity],
            clock_hand: 0,
            // Reversed so frames are handed out in ascending order.
            free_frames: (0..capacity).rev().collect(),
        };
        Ok(BufferPool {
            disk,
            frames,
            state: Mutex::new(state),
        })
    }

    /// Number of frames in the pool.
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// Returns a pinned handle to page `id`, reading it from disk if needed.
    pub fn fetch_page(&self, id: PageId) -> Result<PageHandle<'_>> {
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

    /// Allocates a new page on disk and returns it pinned and dirty.
    pub fn new_page(&self, page_type: PageType) -> Result<PageHandle<'_>> {
        let mut state = self.lock_state();
        // Reserve a frame first so a full pool does not leak a disk page.
        let frame_id = self.acquire_frame(&mut state)?;
        let id = match self.disk.allocate_page() {
            Ok(id) => id,
            Err(err) => {
                state.free_frames.push(frame_id);
                return Err(err);
            }
        };
        *self.write_frame(frame_id) = Frame {
            page: Page::new(id, page_type),
            dirty: true,
        };
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

    /// Writes page `id` to disk if it is cached and dirty. Does not fsync.
    pub fn flush_page(&self, id: PageId) -> Result<()> {
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

    /// Writes every dirty cached page to disk and fsyncs the file.
    pub fn flush_all(&self) -> Result<()> {
        let cached: Vec<PageId> = self.lock_state().page_table.keys().copied().collect();
        for id in cached {
            self.flush_page(id)?;
        }
        self.disk.sync()
    }

    /// Returns an empty, unpinned frame, evicting a page if necessary.
    fn acquire_frame(&self, state: &mut PoolState) -> Result<FrameId> {
        if let Some(frame_id) = state.free_frames.pop() {
            return Ok(frame_id);
        }
        let victim = Self::find_victim(state).ok_or_else(|| {
            Error::ResourceExhausted(format!(
                "all {} buffer pool frames are pinned",
                self.capacity()
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
    pool: &'a BufferPool,
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
    use crate::test_util::TempDir;

    /// Creates a database with `pages` heap pages whose payloads are filled
    /// with their page number.
    fn setup(pages: u64) -> (TempDir, Arc<DiskManager>) {
        let dir = TempDir::new();
        let disk = DiskManager::create(&dir.path().join("db.oxen")).unwrap();
        for _ in 0..pages {
            let id = disk.allocate_page().unwrap();
            let mut page = Page::new(id, PageType::Heap);
            page.payload_mut().fill(id.0 as u8);
            disk.write_page(id, &mut page).unwrap();
        }
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
        assert_eq!(pool.lock_state().pin_counts[a.frame_id], 2);
    }

    #[test]
    fn dropping_handles_unpins() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let handle = pool.fetch_page(PageId(1)).unwrap();
        let frame_id = handle.frame_id;
        let second = pool.fetch_page(PageId(1)).unwrap();
        drop(handle);
        assert_eq!(pool.lock_state().pin_counts[frame_id], 1);
        drop(second);
        assert_eq!(pool.lock_state().pin_counts[frame_id], 0);
    }

    #[test]
    fn write_guard_marks_frame_dirty() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(disk, 4).unwrap();
        let handle = pool.fetch_page(PageId(1)).unwrap();
        assert!(!pool.read_frame(handle.frame_id).dirty);
        drop(handle.write());
        assert!(pool.read_frame(handle.frame_id).dirty);
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
        assert_eq!(pool.lock_state().page_table.len(), 2);
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
            pool.lock_state().page_table.get(&PageId(1)),
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
        assert!(!pool.read_frame(handle.frame_id).dirty);
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
        assert!(pool.lock_state().pin_counts.iter().all(|&pins| pins == 0));
    }

    #[test]
    fn new_page_is_persisted_with_its_type() {
        let (dir, disk) = setup(0);
        let id = {
            let pool = BufferPool::new(disk, 2).unwrap();
            let handle = pool.new_page(PageType::Heap).unwrap();
            handle.write().payload_mut()[0] = 7;
            let id = handle.page_id();
            drop(handle);
            pool.flush_all().unwrap();
            id
        };
        let page = reopen_disk(&dir).read_page(id).unwrap();
        assert_eq!(page.verify(id).unwrap().page_type, PageType::Heap);
        assert_eq!(page.payload()[0], 7);
    }

    #[test]
    fn new_page_on_full_pool_does_not_allocate() {
        let (_dir, disk) = setup(1);
        let pool = BufferPool::new(Arc::clone(&disk), 1).unwrap();
        let _held = pool.fetch_page(PageId(1)).unwrap();
        assert!(matches!(
            pool.new_page(PageType::Heap),
            Err(Error::ResourceExhausted(_))
        ));
        assert_eq!(disk.page_count(), 2);
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
        assert!(pool.read_frame(handle.frame_id).dirty);
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
