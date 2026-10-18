// SPDX-License-Identifier: MPL-2.0

//! Bounded DMA arena caching without reserving the full capacity at startup.

use alloc::{sync::Arc, vec::Vec};

use ostd::{
    Result,
    mm::{PAGE_SIZE, dma::DmaDirection},
    sync::{LocalIrqDisabled, SpinLock},
};

use crate::{DmaArena, DmaArenaPool};

/// A bounded cache of DMA arenas allocated on demand.
///
/// Allocation can create DMA mappings and requires IRQs to be enabled. Returning
/// an allocation remains valid in interrupt context. Idle arenas are reclaimed
/// by later allocations, retaining at most one appropriately sized idle arena.
#[derive(Debug)]
pub struct DmaArenaCache<D: DmaDirection> {
    max_pages: usize,
    chunk_pages: usize,
    state: SpinLock<State<D>, LocalIrqDisabled>,
}

#[derive(Debug)]
struct State<D: DmaDirection> {
    arenas: Vec<CachedArena<D>>,
    pages: usize,
    reserved: usize,
}

#[derive(Debug)]
struct CachedArena<D: DmaDirection> {
    pool: Arc<DmaArenaPool<D>>,
    pages: usize,
}

impl<D: DmaDirection> DmaArenaCache<D> {
    /// Creates an empty cache with a maximum mapped capacity and growth quantum.
    pub fn new(max_pages: usize, chunk_pages: usize) -> Result<Arc<Self>> {
        if chunk_pages == 0 || chunk_pages > max_pages || max_pages.checked_mul(PAGE_SIZE).is_none()
        {
            return Err(ostd::Error::InvalidArgs);
        }
        Ok(Arc::new(Self {
            max_pages,
            chunk_pages,
            state: SpinLock::new(State {
                arenas: Vec::new(),
                pages: 0,
                reserved: 0,
            }),
        }))
    }

    /// Allocates contiguous DMA pages, or returns `None` if the cache cannot serve the request.
    pub fn alloc(&self, size_pages: usize) -> Option<DmaArena<D>> {
        if size_pages == 0 {
            return None;
        }
        let (existing, retired, growth) = {
            let mut state = self.state.lock();
            let mut retired = Vec::new();
            let mut kept_idle = false;
            let mut index = 0;
            while index < state.arenas.len() {
                let arena = &state.arenas[index];
                if Arc::strong_count(&arena.pool) != 1 {
                    index += 1;
                    continue;
                }
                if !kept_idle
                    && arena.pages >= size_pages
                    && arena.pages <= self.chunk_pages.max(size_pages)
                {
                    kept_idle = true;
                    index += 1;
                } else {
                    let removed = state.arenas.swap_remove(index);
                    state.pages -= removed.pages;
                    retired.push(removed);
                }
            }
            let existing = state
                .arenas
                .iter()
                .find_map(|arena| arena.pool.alloc(size_pages));
            let available = self.max_pages - state.pages - state.reserved;
            let growth = if existing.is_none() && size_pages <= available {
                let pages = self.chunk_pages.max(size_pages).min(available);
                state.reserved += pages;
                pages
            } else {
                0
            };
            (existing, retired, growth)
        };
        // DMA mapping destruction and creation run after releasing the cache lock.
        drop(retired);
        if existing.is_some() {
            return existing;
        }
        if growth == 0 {
            return None;
        }
        let pool = match DmaArenaPool::new(growth) {
            Ok(pool) => pool,
            Err(_) => {
                self.state.lock().reserved -= growth;
                return None;
            }
        };
        let arena = pool
            .alloc(size_pages)
            .expect("a new DMA arena must fit its initial request");
        let mut state = self.state.lock();
        state.reserved -= growth;
        state.pages += growth;
        state.arenas.push(CachedArena {
            pool,
            pages: growth,
        });
        Some(arena)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::{
        irq,
        mm::{HasDaddr, HasSize, dma::FromAndToDevice},
        prelude::ktest,
    };

    use super::*;

    #[ktest]
    fn on_demand_dma_cache_regressions() {
        assert!(DmaArenaCache::<FromAndToDevice>::new(0, 1).is_err());
        assert!(DmaArenaCache::<FromAndToDevice>::new(12, 0).is_err());
        assert!(DmaArenaCache::<FromAndToDevice>::new(12, 13).is_err());
        assert!(DmaArenaCache::<FromAndToDevice>::new(usize::MAX, 1).is_err());
        let cache = DmaArenaCache::<FromAndToDevice>::new(12, 4).unwrap();
        assert_eq!(cache.state.lock().pages, 0);
        let first = cache.alloc(1).unwrap();
        let address = first.daddr();
        assert_eq!(cache.state.lock().pages, 4);
        {
            let _guard = irq::disable_local();
            drop(first);
        }
        let reused = cache.alloc(1).unwrap();
        assert_eq!(reused.daddr(), address);
        drop(reused);
        let held: Vec<_> = (0..12).map(|_| cache.alloc(1).unwrap()).collect();
        assert_eq!(cache.state.lock().pages, 12);
        assert!(cache.alloc(1).is_none());
        assert!(held.iter().all(|arena| arena.size() == PAGE_SIZE));
        drop(held);
        let small = cache.alloc(1).unwrap();
        assert_eq!(cache.state.lock().pages, 4);
        drop(small);
        let entire = cache.alloc(12).unwrap();
        assert_eq!(entire.size(), 12 * PAGE_SIZE);
        assert_eq!(cache.state.lock().pages, 12);
        assert!(cache.alloc(1).is_none());
        drop(entire);
        let small = cache.alloc(1).unwrap();
        assert_eq!(cache.state.lock().pages, 4);
        drop(small);
        let full_cache = DmaArenaCache::<FromAndToDevice>::new(4096, 64).unwrap();
        let warm = full_cache.alloc(1).unwrap();
        drop(warm);
        let full = full_cache.alloc(4096).unwrap();
        assert_eq!(full.size(), 16 * 1024 * 1024);
        assert_eq!(full_cache.state.lock().pages, 4096);
        assert!(full_cache.alloc(1).is_none());
    }
}
