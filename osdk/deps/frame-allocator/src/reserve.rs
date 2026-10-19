// SPDX-License-Identifier: MPL-2.0

//! Frames kept available for kernel progress when application memory is exhausted.

use core::{
    alloc::Layout,
    sync::atomic::{AtomicUsize, Ordering},
};

use ostd::{
    mm::{PAGE_SIZE, Paddr},
    sync::{LocalIrqDisabled, SpinLock},
};

use crate::{
    chunk::{greater_order_of, size_of_order, split_to_chunks},
    set::BuddySet,
};

const RESERVE_BYTES: usize = 8 * 1024 * 1024;
const RESERVE_ORDERS: usize = (RESERVE_BYTES / PAGE_SIZE).ilog2() as usize + 1;
static START: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);
static POOL: SpinLock<BuddySet<RESERVE_ORDERS>, LocalIrqDisabled> =
    SpinLock::new(BuddySet::new_empty());

/// Adds reserved frames and returns ranges that belong to the ordinary pools.
pub(super) fn add_free_memory(addr: Paddr, size: usize) -> [Option<(Paddr, usize)>; 2] {
    if START.load(Ordering::Acquire) == 0 && size >= RESERVE_BYTES {
        let mut pool = POOL.lock();
        if START.load(Ordering::Relaxed) == 0 {
            let Some(end) = addr.checked_add(RESERVE_BYTES) else {
                return [Some((addr, size)), None];
            };
            for (addr, order) in split_to_chunks(addr, RESERVE_BYTES) {
                pool.insert_chunk(addr, order);
            }
            END.store(end, Ordering::Relaxed);
            START.store(addr, Ordering::Release);
            return [
                (size > RESERVE_BYTES).then_some((end, size - RESERVE_BYTES)),
                None,
            ];
        }
    }
    dealloc(addr, size)
}

pub(super) fn alloc(layout: Layout) -> Option<Paddr> {
    let order = greater_order_of(layout.size()).max(greater_order_of(layout.align()));
    if order >= RESERVE_ORDERS {
        return None;
    }
    let mut pool = POOL.lock();
    let addr = pool.alloc_chunk(order)?;
    let allocated = size_of_order(order);
    if allocated > layout.size() {
        for (addr, order) in split_to_chunks(addr + layout.size(), allocated - layout.size()) {
            pool.insert_chunk(addr, order);
        }
    }
    Some(addr)
}

/// Returns released ordinary ranges after putting any reserve overlap back.
pub(super) fn dealloc(addr: Paddr, size: usize) -> [Option<(Paddr, usize)>; 2] {
    let start = START.load(Ordering::Acquire);
    let end = END.load(Ordering::Relaxed);
    let Some(released_end) = addr.checked_add(size) else {
        return [Some((addr, size)), None];
    };
    if start == 0 || released_end <= start || addr >= end {
        return [Some((addr, size)), None];
    }
    let overlap_start = addr.max(start);
    let overlap_end = released_end.min(end);
    let mut pool = POOL.lock();
    for (addr, order) in split_to_chunks(overlap_start, overlap_end - overlap_start) {
        pool.insert_chunk(addr, order);
    }
    [
        (addr < overlap_start).then_some((addr, overlap_start - addr)),
        (overlap_end < released_end).then_some((overlap_end, released_end - overlap_end)),
    ]
}
