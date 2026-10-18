// SPDX-License-Identifier: MPL-2.0

//! Ownership of the boot archive and its reclaimable physical pages.

use core::ops::{Deref, Range};

use crate::{
    impl_frame_meta_for,
    mm::{
        PAGE_SIZE, Paddr, UniqueFrame, frame::allocator::get_global_frame_allocator, paddr_to_vaddr,
    },
};

/// An owned boot archive. Byte borrows cannot outlive its physical pages.
pub struct InitRamfs {
    range: Range<Paddr>,
    _frames: Option<InitRamfsFrames>,
}

#[derive(Debug)]
struct InitRamfsFrameMeta;
impl_frame_meta_for!(InitRamfsFrameMeta);

struct InitRamfsFrames {
    // Each page owns a forgotten `UniqueFrame<InitRamfsFrameMeta>`.
    range: Range<Paddr>,
}

impl InitRamfsFrames {
    #[cfg(feature = "pvh_boot_only")]
    fn new(range: Range<Paddr>) -> Self {
        for paddr in range.clone().step_by(PAGE_SIZE) {
            UniqueFrame::from_unused(paddr, InitRamfsFrameMeta)
                .unwrap()
                .into_raw();
        }
        Self { range }
    }
}

impl Drop for InitRamfsFrames {
    fn drop(&mut self) {
        // SAFETY:
        // 1. The unique typed frames cannot be shared or mapped as user data.
        // 2. Archive borrows have ended before drop. Clear only the full pages
        //    owned by this range, leaving shared edge pages untouched.
        unsafe {
            core::ptr::write_bytes(
                paddr_to_vaddr(self.range.start) as *mut u8,
                0,
                self.range.len(),
            );
        }
        for paddr in self.range.clone().step_by(PAGE_SIZE) {
            // SAFETY: `new` forgot exactly one unique handle per page. This
            // non-cloneable owner restores each handle once; none were shared.
            let frame = unsafe { UniqueFrame::<InitRamfsFrameMeta>::from_raw(paddr) };
            frame.reset_as_unused();
        }
        // Boot module pages were never added to the allocator. Register them
        // once, after their unique metadata has been removed.
        get_global_frame_allocator().add_free_memory(self.range.start, self.range.len());
    }
}

static EARLY_FRAMES: spin::Mutex<Option<InitRamfsFrames>> = spin::Mutex::new(None);

impl InitRamfs {
    pub(super) fn new(range: Range<Paddr>) -> Self {
        Self {
            range,
            _frames: EARLY_FRAMES.lock().take(),
        }
    }
}

impl Deref for InitRamfs {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY:
        // 1. Boot parsing reserves a valid, readable loader-provided archive.
        // 2. Its full pages are uniquely owned by `_frames` or permanently
        //    reserved. Partial pages stay reserved. This borrow cannot outlive
        //    the owner that returns the pages.
        unsafe {
            core::slice::from_raw_parts(
                paddr_to_vaddr(self.range.start) as *const u8,
                self.range.end - self.range.start,
            )
        }
    }
}

#[cfg(feature = "pvh_boot_only")]
pub(crate) fn reserve_frames(region: &super::memory_region::MemoryRegion) -> Option<Range<Paddr>> {
    use crate::mm::kspace::{LINEAR_MAPPING_BASE_VADDR, LINEAR_MAPPING_VADDR_RANGE};

    let info = super::EARLY_INFO.get().unwrap();
    let archive = info.initramfs.as_ref()?;
    let protected = [
        info.kernel_cmdline.as_bytes(),
        info.bootloader_name.as_bytes(),
    ]
    .map(|bytes| {
        let address = bytes.as_ptr() as usize;
        if !bytes.is_empty() && LINEAR_MAPPING_VADDR_RANGE.contains(&address) {
            let start = address - LINEAR_MAPPING_BASE_VADDR;
            start..start + bytes.len()
        } else {
            0..0
        }
    });
    let range = reclaimable_range(archive, region, &protected)?;
    let mut frames = EARLY_FRAMES.lock();
    assert!(frames.is_none());
    *frames = Some(InitRamfsFrames::new(range.clone()));
    Some(range)
}

#[cfg(feature = "pvh_boot_only")]
fn reclaimable_range(
    archive: &Range<Paddr>,
    region: &super::memory_region::MemoryRegion,
    protected: &[Range<Paddr>],
) -> Option<Range<Paddr>> {
    use align_ext::AlignExt;

    use super::memory_region::MemoryRegionType;

    if !matches!(
        region.typ(),
        MemoryRegionType::Module | MemoryRegionType::Reclaimable
    ) {
        return None;
    }
    let start = archive
        .start
        .checked_add(PAGE_SIZE - 1)?
        .align_down(PAGE_SIZE);
    let end = archive.end.align_down(PAGE_SIZE);
    if start >= end || start < region.base() || end > region.end() {
        return None;
    }
    if protected
        .iter()
        .any(|range| !range.is_empty() && range.start < end && start < range.end)
    {
        return None;
    }
    Some(start..end)
}

#[cfg(all(ktest, feature = "pvh_boot_only"))]
mod test {
    use super::*;
    use crate::{
        boot::memory_region::{MemoryRegion, MemoryRegionType},
        prelude::ktest,
    };

    #[ktest]
    fn preserves_edges_and_shared_pages() {
        let page = PAGE_SIZE;
        let region = MemoryRegion::new(page, 4 * page, MemoryRegionType::Reclaimable);
        let archive = page + 1..5 * page - 1;
        assert_eq!(
            reclaimable_range(&archive, &region, &[]),
            Some(2 * page..4 * page)
        );
        assert_eq!(reclaimable_range(&(page + 1..2 * page), &region, &[]), None);
        let shared = 3 * page..3 * page + 1;
        assert_eq!(
            reclaimable_range(&archive, &region, core::slice::from_ref(&shared)),
            None
        );
        let edge = page..page + 1;
        assert_eq!(
            reclaimable_range(&archive, &region, core::slice::from_ref(&edge)),
            Some(2 * page..4 * page)
        );
    }

    #[ktest]
    fn requires_one_reserved_module_region() {
        let page = PAGE_SIZE;
        let archive = page..4 * page;
        for typ in [
            MemoryRegionType::Kernel,
            MemoryRegionType::Reserved,
            MemoryRegionType::Framebuffer,
            MemoryRegionType::Usable,
        ] {
            assert_eq!(
                reclaimable_range(&archive, &MemoryRegion::new(page, 3 * page, typ), &[]),
                None
            );
        }
        for typ in [MemoryRegionType::Module, MemoryRegionType::Reclaimable] {
            assert_eq!(
                reclaimable_range(&archive, &MemoryRegion::new(page, 2 * page, typ), &[]),
                None
            );
            assert_eq!(
                reclaimable_range(&archive, &MemoryRegion::new(page, 3 * page, typ), &[]),
                Some(archive.clone())
            );
        }
    }
}
