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
//! Holding `state` during miss I/O serializes misses. It keeps the
//! implementation simple and easy to reason about; lifting it requires an
//! "I/O in progress" frame state and should be justified by a benchmark.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::{Error, Result};
use crate::storage::disk::DiskManager;
use crate::storage::page::{Page, PageId};

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
            return Ok(PageHandle {
                pool: self,
                frame_id,
                page_id: id,
            });
        }

        let frame_id = state.free_frames.pop().ok_or_else(|| {
            Error::ResourceExhausted(format!(
                "all {} buffer pool frames are in use",
                self.capacity()
            ))
        })?;
        let page = match self.disk.read_page(id) {
            Ok(page) => page,
            Err(err) => {
                state.free_frames.push(frame_id);
                return Err(err);
            }
        };
        // The frame was free, so no handle can be holding its lock.
        *self.write_frame(frame_id) = Frame { page, dirty: false };
        state.page_table.insert(id, frame_id);
        state.frame_pages[frame_id] = Some(id);
        state.pin_counts[frame_id] = 1;
        Ok(PageHandle {
            pool: self,
            frame_id,
            page_id: id,
        })
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
    use crate::storage::page::PageType;
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
