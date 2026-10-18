// SPDX-License-Identifier: MPL-2.0

//! Submission failures must retain every unsubmitted dirty page.

use ostd::{mm::VmIo, prelude::ktest};

use super::{PageCache, PageCacheBackend};
use crate::{prelude::*, vm::page_cache::LockedCachePage};

struct FailFirstWrite {
    fail: core::sync::atomic::AtomicBool,
    writes: [core::sync::atomic::AtomicUsize; 4],
}

impl PageCacheBackend for FailFirstWrite {
    fn read_page_async(
        &self,
        _idx: usize,
        page: LockedCachePage,
        _batch: &mut io_util::batch::IoBatch,
    ) -> Result<()> {
        page.write_bytes(0, &vec![0; PAGE_SIZE])?;
        page.set_up_to_date();
        Ok(())
    }
    fn write_page_async(
        &self,
        idx: usize,
        page: LockedCachePage,
        _batch: &mut io_util::batch::IoBatch,
    ) -> Result<()> {
        use core::sync::atomic::Ordering;
        if self.fail.swap(false, Ordering::AcqRel) {
            page.set_dirty();
            return_errno!(Errno::EIO);
        }
        self.writes[idx].fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[ktest]
fn writeback_submission_failure_preserves_later_runs() {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    for dirty in [&[0, 2][..], &[0, 1, 3][..]] {
        let backend = Arc::new(FailFirstWrite {
            fail: AtomicBool::new(true),
            writes: core::array::from_fn(|_| AtomicUsize::new(0)),
        });
        let backend_dyn: Arc<dyn PageCacheBackend> = backend.clone();
        let cache =
            PageCache::new_with_backend(4 * PAGE_SIZE, Arc::downgrade(&backend_dyn)).unwrap();
        for &idx in dirty {
            cache
                .write_bytes(idx * PAGE_SIZE, &vec![0x57; PAGE_SIZE])
                .unwrap();
        }
        assert_eq!(
            cache.flush_range(0..4 * PAGE_SIZE).unwrap_err().error(),
            Errno::EIO
        );
        cache.flush_range(0..4 * PAGE_SIZE).unwrap();
        for &idx in dirty {
            assert_eq!(
                backend.writes[idx].load(Ordering::Relaxed),
                1,
                "page {} lost after failed writeback",
                idx
            );
        }
    }
}
