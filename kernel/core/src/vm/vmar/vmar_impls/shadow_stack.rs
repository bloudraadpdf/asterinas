// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use ostd::{
    arch::cpu::context::write_shadow_stack_word,
    mm::{
        AllocationPriority, CachePolicy, FrameAllocOptions, PageFlags, PageProperty, VmIo, VmSpace,
        io::util::HasVmReaderWriter, tlb::TlbFlushOp, vm_space::VmQueriedItem,
    },
    task::disable_preempt,
};

use super::{RssDelta, VmMapping, Vmar, VmarInner, map::VmarMapOffset, page_fault::PageFaultInfo};
use crate::{
    prelude::*,
    process::INIT_STACK_SIZE,
    vm::{
        perms::VmPerms,
        vmar::{VMAR_CAP_ADDR, vm_mapping::duplicate_frame},
    },
};

const SHADOW_STACK_MIN_ADDR: Vaddr = 1 << 32;

impl Vmar {
    pub(crate) fn map_shadow_stack(&self, address: Vaddr, size: usize) -> Result<Vaddr> {
        if size == 0 || !size.is_multiple_of(PAGE_SIZE) || !address.is_multiple_of(PAGE_SIZE) {
            return_errno!(Errno::EINVAL);
        }
        if address != 0 && address < SHADOW_STACK_MIN_ADDR {
            return_errno!(Errno::ERANGE);
        }
        if address
            .checked_add(size)
            .is_none_or(|end| end > VMAR_CAP_ADDR)
        {
            return_errno!(Errno::ENOMEM);
        }
        let offset = if address == 0 {
            VmarMapOffset::Any
        } else {
            VmarMapOffset::FixedNoReplace(address)
        };
        self.new_map(size, VmPerms::READ | VmPerms::WRITE)
            .shadow_stack()
            .offset(offset)
            .build()
    }

    fn with_user_word_mapping<T>(
        &self,
        address: Vaddr,
        operate: impl FnOnce(&VmMapping) -> Result<T>,
    ) -> Result<T> {
        if !address.is_multiple_of(8)
            || address.checked_add(8).is_none_or(|end| end > VMAR_CAP_ADDR)
        {
            return_errno!(Errno::EFAULT);
        }
        let inner = self.inner.read();
        let mapping = inner
            .vm_mappings
            .find_one(&address)
            .ok_or_else(|| Error::new(Errno::EFAULT))?;
        operate(mapping)
    }

    pub(crate) fn read_shadow_stack_signal_token(&self, address: Vaddr) -> Result<u64> {
        self.with_user_word_mapping(address, |mapping| {
            // INCSSP reads the last popped element. Only a token at the beginning
            // of a page needs a separate VMA identity check, matching Linux shstk.c.
            if address.is_multiple_of(PAGE_SIZE) && !mapping.is_shadow_stack() {
                return_errno!(Errno::EFAULT);
            }
            let info = PageFaultInfo::new(address, VmPerms::READ);
            let range = address / PAGE_SIZE * PAGE_SIZE..(address / PAGE_SIZE + 1) * PAGE_SIZE;
            if !mapping.perms().contains(VmPerms::READ) {
                return_errno!(Errno::EFAULT);
            }
            loop {
                {
                    let guard = disable_preempt();
                    let mut cursor = self.vm_space.cursor_mut(&guard, &range).unwrap();
                    match cursor.query().unwrap().1 {
                        Some(VmQueriedItem::MappedRam { frame, prop })
                            if prop.flags.contains(PageFlags::R) =>
                        {
                            return frame
                                .reader()
                                .skip(address % PAGE_SIZE)
                                .read_val()
                                .map_err(Error::from);
                        }
                        Some(VmQueriedItem::MappedIoMem { paddr, prop })
                            if prop.flags.contains(PageFlags::R) =>
                        {
                            let (memory, offset) = cursor
                                .find_iomem_by_paddr(paddr)
                                .ok_or_else(|| Error::new(Errno::EFAULT))?;
                            let offset = offset
                                .checked_add(address % PAGE_SIZE)
                                .ok_or_else(|| Error::new(Errno::EFAULT))?;
                            return memory.read_val(offset).map_err(Error::from);
                        }
                        _ => {}
                    }
                }
                let mut rss = RssDelta::new(self);
                self.fault_locked_mapping(mapping, &info, &mut rss)?;
            }
        })
    }

    pub(crate) fn write_shadow_stack_word(&self, address: Vaddr, value: u64) -> Result<()> {
        self.with_user_word_mapping(address, |mapping| {
            if !mapping.is_shadow_stack() {
                return_errno!(Errno::EFAULT);
            }
            let info = PageFaultInfo::new(address, VmPerms::READ | VmPerms::WRITE).shadow_stack();
            let page_address = address / PAGE_SIZE * PAGE_SIZE;
            loop {
                let mut rss = RssDelta::new(self);
                self.fault_locked_mapping(mapping, &info, &mut rss)?;
                let guard = disable_preempt();
                let range = page_address..page_address + PAGE_SIZE;
                let mut cursor = self.vm_space.cursor_mut(&guard, &range).unwrap();
                let (_, Some(VmQueriedItem::MappedRam { frame, prop })) = cursor.query().unwrap()
                else {
                    continue;
                };
                if !prop
                    .flags
                    .contains(PageFlags::SHADOW_STACK | PageFlags::DIRTY)
                {
                    continue;
                }
                // Holding the page-table lock prevents fork from write-protecting this page
                // between the COW resolution above and the physical-alias write.
                return write_shadow_stack_word(&frame, address - page_address, value)
                    .map_err(|_| Error::new(Errno::EFAULT));
            }
        })
    }
}

impl VmarInner {
    pub(super) fn alloc_shadow_stack_region(&mut self, size: usize) -> Result<Range<Vaddr>> {
        let reservation = size
            .checked_add(PAGE_SIZE)
            .ok_or_else(|| Error::new(Errno::ENOMEM))?;
        let high = VMAR_CAP_ADDR - INIT_STACK_SIZE - PAGE_SIZE * 2048;
        let region =
            self.alloc_free_region_in_range(reservation, PAGE_SIZE, SHADOW_STACK_MIN_ADDR, high)?;
        Ok(region.start + PAGE_SIZE..region.end)
    }
}

impl VmMapping {
    pub(in crate::vm::vmar) fn handle_shadow_stack_fault(
        &self,
        vm_space: &VmSpace,
        info: &PageFaultInfo,
        rss: &mut RssDelta,
    ) -> Result<()> {
        let address = info.address() / PAGE_SIZE * PAGE_SIZE;
        let guard = disable_preempt();
        let range = address..address + PAGE_SIZE;
        let mut cursor = vm_space.cursor_mut(&guard, &range)?;
        let (mapped_range, item) = cursor.query().unwrap();
        let write = info.is_shadow_stack();
        match item {
            Some(VmQueriedItem::MappedRam { frame, mut prop }) => {
                if write && !prop.flags.contains(PageFlags::DIRTY) {
                    let flags = (prop.flags
                        | PageFlags::ACCESSED
                        | PageFlags::DIRTY
                        | PageFlags::SHADOW_STACK)
                        - PageFlags::W;
                    if frame.reference_count() == 1 {
                        cursor.protect_next(PAGE_SIZE, |old, _| *old = flags);
                        cursor
                            .flusher()
                            .issue_tlb_flush(TlbFlushOp::for_range(mapped_range));
                        cursor.flusher().dispatch_tlb_flush();
                    } else {
                        let copied = duplicate_frame(&frame)?;
                        prop.flags = flags;
                        cursor.unmap(PAGE_SIZE);
                        cursor.jump(address).unwrap();
                        cursor.map(copied.into(), prop);
                    }
                    cursor.flusher().sync_tlb_flush();
                } else {
                    TlbFlushOp::for_range(mapped_range).perform_on_current();
                }
            }
            Some(VmQueriedItem::MappedIoMem { .. }) => return_errno!(Errno::EFAULT),
            None => {
                let frame = FrameAllocOptions::new()
                    .priority(AllocationPriority::Normal)
                    .alloc_frame()?;
                let mut flags = PageFlags::from(self.perms()) - PageFlags::W;
                flags |= PageFlags::SHADOW_STACK | PageFlags::ACCESSED;
                if self.perms().contains(VmPerms::WRITE) {
                    flags |= PageFlags::DIRTY;
                }
                cursor.map(
                    frame.into(),
                    PageProperty::new_user(flags, CachePolicy::Writeback),
                );
                rss.add(self.rss_type(), 1);
            }
        }
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{fork::cow_copy_pt, protect::ProtectDirection},
        *,
    };
    use crate::{fs::pseudofs::SockFs, process::ProcessVm, vm::vmar::VmarHandle};

    fn new_vmar() -> VmarHandle {
        crate::time::clocks::init_for_ktest();
        crate::util::random::init();
        VmarHandle::new(ProcessVm::new(SockFs::new_path()))
    }

    fn read_word(vmar: &Vmar, address: Vaddr) -> u64 {
        let guard = disable_preempt();
        let range = address / PAGE_SIZE * PAGE_SIZE..(address / PAGE_SIZE + 1) * PAGE_SIZE;
        let mut cursor = vmar.vm_space.cursor(&guard, &range).unwrap();
        let (_, Some(VmQueriedItem::MappedRam { frame, prop })) = cursor.query().unwrap() else {
            panic!("missing shadow page");
        };
        assert!(prop.flags.contains(PageFlags::SHADOW_STACK));
        assert!(!prop.flags.contains(PageFlags::W));
        frame.reader().skip(address % PAGE_SIZE).read_val().unwrap()
    }

    #[ktest]
    fn shadow_mapping_is_lazy_guarded_and_rejects_ordinary_writes() {
        let vmar = new_vmar();
        assert_eq!(
            vmar.map_shadow_stack(PAGE_SIZE, PAGE_SIZE)
                .unwrap_err()
                .error(),
            Errno::ERANGE
        );
        assert_eq!(
            vmar.map_shadow_stack(0, 0).unwrap_err().error(),
            Errno::EINVAL
        );
        let base = vmar.map_shadow_stack(0, PAGE_SIZE).unwrap();
        assert!(base >= SHADOW_STACK_MIN_ADDR);
        assert!(
            vmar.query(base..base + PAGE_SIZE)
                .iter()
                .all(VmMapping::is_shadow_stack)
        );
        assert!(vmar.query(base - PAGE_SIZE..base).iter().next().is_none());
        {
            let guard = disable_preempt();
            assert!(
                vmar.vm_space
                    .cursor(&guard, &(base..base + PAGE_SIZE))
                    .unwrap()
                    .query()
                    .unwrap()
                    .1
                    .is_none()
            );
        }
        let hint = vmar
            .new_map(PAGE_SIZE, VmPerms::READ)
            .offset(VmarMapOffset::Hint(base - PAGE_SIZE))
            .build()
            .unwrap();
        assert_ne!(hint, base - PAGE_SIZE);
        let ordinary_write = PageFaultInfo::new(base, VmPerms::WRITE);
        assert_eq!(
            vmar.handle_page_fault(&ordinary_write).unwrap_err().error(),
            Errno::EACCES
        );
        assert_eq!(
            vmar.write_shadow_stack_word(hint, 1).unwrap_err().error(),
            Errno::EFAULT
        );
        vmar.write_shadow_stack_word(base + PAGE_SIZE - 8, 0x1234)
            .unwrap();
        assert_eq!(read_word(&vmar, base + PAGE_SIZE - 8), 0x1234);
        vmar.protect(
            VmPerms::READ,
            base..base + PAGE_SIZE,
            ProtectDirection::None,
        )
        .unwrap();
        assert!(
            vmar.query(base..base + PAGE_SIZE)
                .iter()
                .all(VmMapping::is_shadow_stack)
        );
        assert_eq!(
            vmar.write_shadow_stack_word(base, 1).unwrap_err().error(),
            Errno::EACCES
        );
        vmar.protect(
            VmPerms::READ | VmPerms::WRITE,
            base..base + PAGE_SIZE,
            ProtectDirection::None,
        )
        .unwrap();
        assert_eq!(
            vmar.handle_page_fault(&ordinary_write).unwrap_err().error(),
            Errno::EACCES
        );
        vmar.write_shadow_stack_word(base, 0x5678).unwrap();
        assert_eq!(read_word(&vmar, base), 0x5678);
        assert_eq!(
            vmar.map_shadow_stack(base, PAGE_SIZE).unwrap_err().error(),
            Errno::EEXIST
        );
    }

    #[ktest]
    fn shadow_stack_copy_on_write_preserves_parent_and_child_tokens() {
        let parent = new_vmar();
        let child = new_vmar();
        let base = parent.map_shadow_stack(0, PAGE_SIZE).unwrap();
        parent.write_shadow_stack_word(base, 11).unwrap();
        {
            let inner = parent.inner.read();
            let mapping = inner.vm_mappings.find_one(&base).unwrap().new_fork();
            child
                .inner
                .write()
                .insert_without_try_merge(&child, mapping, None);
        }
        {
            let guard = disable_preempt();
            let range = base..base + PAGE_SIZE;
            let mut source = parent.vm_space.cursor_mut(&guard, &range).unwrap();
            let mut target = child.vm_space.cursor_mut(&guard, &range).unwrap();
            assert_eq!(cow_copy_pt(&mut source, &mut target, PAGE_SIZE), 1);
            let mut rss = RssDelta::new(&child);
            rss.add(super::super::RssType::Anon, 1);
        }
        child.write_shadow_stack_word(base, 22).unwrap();
        assert_eq!(read_word(&parent, base), 11);
        assert_eq!(read_word(&child, base), 22);
        parent.write_shadow_stack_word(base, 33).unwrap();
        assert_eq!(read_word(&parent, base), 33);
        assert_eq!(read_word(&child, base), 22);
    }
}
