// SPDX-License-Identifier: MPL-2.0

use core::{mem, ops::Range};

use ostd::{
    mm::{Frame, vm_space::VmQueriedItem},
    task::{Task, disable_preempt},
};

use super::{Interval, RssDelta, VmMapping, Vmar, VmarInner, util::get_intersected_range};
use crate::{
    prelude::*,
    process::{
        Process, ResourceType, UserNamespace, credentials::capabilities::CapSet,
        posix_thread::AsPosixThread,
    },
    security::lsm::hooks as lsm_hooks,
    vm::{
        page_cache::{CachePage, CachePageExt, CachePagePin},
        perms::VmPerms,
        vmar::PageFaultInfo,
    },
};

pub(super) struct LockedPage {
    address: Vaddr,
    pin: Option<CachePagePin>,
}

pub(crate) fn current_lock_limit() -> usize {
    let Some(process) = Process::current() else {
        return usize::MAX;
    };
    let limit = process
        .resource_limits()
        .get_rlimit(ResourceType::RLIMIT_MEMLOCK)
        .get_cur() as usize;
    let Some(task) = Task::current() else {
        return limit;
    };
    let Some(posix) = task.as_posix_thread() else {
        return limit;
    };
    if lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
        UserNamespace::get_init_singleton().as_ref(),
        posix,
        CapSet::IPC_LOCK,
    ))
    .is_ok()
    {
        usize::MAX
    } else {
        limit
    }
}

impl Vmar {
    pub(crate) fn locked_bytes(&self) -> usize {
        self.locked_pages.lock().len() * PAGE_SIZE
    }

    pub(super) fn reserve_locked_pages(
        &self,
        _inner: &mut VmarInner,
        count: usize,
        error: Errno,
    ) -> Result<()> {
        let limit = current_lock_limit() / PAGE_SIZE;
        let mut pages = mem::take(&mut *self.locked_pages.lock());
        let result = if limit.saturating_sub(pages.len()) < count {
            Err(Error::new(error))
        } else {
            pages
                .try_reserve(count)
                .map_err(|_| Error::new(Errno::ENOMEM))
        };
        *self.locked_pages.lock() = pages;
        result
    }

    pub(super) fn add_locked_pages(&self, range: Range<Vaddr>) {
        let mut pages = self.locked_pages.lock();
        debug_assert!(pages.capacity() - pages.len() >= range.len() / PAGE_SIZE);
        for address in range.step_by(PAGE_SIZE) {
            pages.push(LockedPage { address, pin: None });
        }
        pages.sort_unstable_by_key(|page| page.address);
    }

    pub(super) fn release_locked_pages(&self, range: Range<Vaddr>) {
        self.locked_pages
            .lock()
            .retain(|page| !range.contains(&page.address));
    }

    pub(in crate::vm::vmar) fn invalidate_locked_pages(&self, range: Range<Vaddr>) {
        let mut pages = self.locked_pages.lock();
        for page in pages
            .iter_mut()
            .filter(|page| range.contains(&page.address))
        {
            page.pin = None;
        }
    }

    pub(super) fn move_locked_pages(&self, old: Range<Vaddr>, new_start: Vaddr) {
        let mut pages = self.locked_pages.lock();
        for page in pages.iter_mut().filter(|page| old.contains(&page.address)) {
            page.address = new_start + (page.address - old.start);
        }
        pages.sort_unstable_by_key(|page| page.address);
    }

    pub(crate) fn set_memory_lock(&self, range: Range<Vaddr>, locked: bool) -> Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.write();
        let mut position = range.start;
        let mut additional = 0;
        for mapping in inner.vm_mappings.find(&range) {
            if mapping.map_to_addr() > position {
                return_errno!(Errno::ENOMEM);
            }
            let selected = get_intersected_range(&mapping.range(), &range);
            if locked && !mapping.memory_locked() && mapping.can_memory_lock() {
                additional += selected.len() / PAGE_SIZE;
            }
            position = selected.end;
        }
        if position < range.end {
            return_errno!(Errno::ENOMEM);
        }
        if locked {
            self.reserve_locked_pages(&mut inner, additional, Errno::ENOMEM)?;
        }
        if locked {
            for mapping in inner.vm_mappings.find(&range) {
                if !mapping.memory_locked() && mapping.can_memory_lock() {
                    self.add_locked_pages(get_intersected_range(&mapping.range(), &range));
                }
            }
        } else {
            self.release_locked_pages(range.clone());
        }
        inner.set_memory_lock_flags(self, range.clone(), locked);
        drop(inner);
        if locked {
            self.populate_locked(range)?;
        }
        Ok(())
    }

    pub(super) fn populate_locked(&self, range: Range<Vaddr>) -> Result<()> {
        for address in range.step_by(PAGE_SIZE) {
            let inner = self.inner.read();
            let mapping = inner
                .vm_mappings
                .find_one(&address)
                .ok_or_else(|| Error::new(Errno::ENOMEM))?;
            if !mapping.memory_locked() {
                continue;
            }
            let info = lock_fault_info(mapping, address)?;
            let mut rss = RssDelta::new(self);
            self.fault_locked_mapping(mapping, &info, &mut rss)
                .map_err(|error| match error.error() {
                    Errno::ENOMEM => Error::new(Errno::EAGAIN),
                    Errno::EACCES | Errno::EFAULT | Errno::ENXIO | Errno::ENOSPC => {
                        Error::new(Errno::ENOMEM)
                    }
                    _ => error,
                })?;
        }
        Ok(())
    }

    pub(super) fn populate_locked_best_effort(&self, range: Range<Vaddr>) {
        // MAP_LOCKED and committed mapping growth keep the mapping when population fails.
        // Reference: <https://github.com/torvalds/linux/blob/v6.16/mm/mremap.c>.
        let _ = self.populate_locked(range);
    }

    pub(super) fn fault_locked_mapping(
        &self,
        mapping: &VmMapping,
        info: &PageFaultInfo,
        rss: &mut RssDelta,
    ) -> Result<()> {
        loop {
            mapping.handle_page_fault(&self.vm_space, info, rss)?;
            if !mapping.memory_locked() || mapping.vmo_for_rmap().is_none() {
                return Ok(());
            }
            let address = info.address() / PAGE_SIZE * PAGE_SIZE;
            let retry = {
                let preempt_guard = disable_preempt();
                let range = address..address + PAGE_SIZE;
                let mut cursor = self.vm_space.cursor(&preempt_guard, &range).unwrap();
                let (_, Some(VmQueriedItem::MappedRam { frame, .. })) = cursor.query().unwrap()
                else {
                    // An invalidation may remove the PTE after the fault returns.
                    // Fault again before claiming that this locked page is resident.
                    continue;
                };
                let Ok(page) = CachePage::try_from(Frame::from_unsized((*frame).clone())) else {
                    return Ok(());
                };
                match CachePagePin::try_new(page) {
                    Ok(pin) => {
                        let mut pages = self.locked_pages.lock();
                        let index = pages
                            .binary_search_by_key(&address, |page| page.address)
                            .expect("locked mapping has a pin slot");
                        pages[index].pin = Some(pin);
                        return Ok(());
                    }
                    Err(page) => page,
                }
            };
            // Eviction owns the page lock until the old PTE has been removed.
            drop(retry.lock_guard());
        }
    }
}

impl VmarInner {
    pub(super) fn set_memory_lock_flags(&mut self, vmar: &Vmar, range: Range<Vaddr>, locked: bool) {
        let mut position = range.start;
        while position < range.end {
            let mapping = self.vm_mappings.find_one(&position).unwrap();
            let mapping_range = mapping.range();
            let selected_range = get_intersected_range(&mapping_range, &(position..range.end));
            position = selected_range.end;
            if !mapping.can_memory_lock() || mapping.memory_locked() == locked {
                continue;
            }
            let (mapping, mut removed) = self.remove(&mapping_range.start).unwrap();
            let mut rmap = removed.remove(vmar, mapping_range.start);
            let mut selected =
                self.extract_mapping_range(vmar, mapping, &selected_range, rmap.as_deref_mut());
            selected.set_memory_locked(locked);
            self.insert_try_merge(vmar, selected, rmap.as_deref_mut());
        }
    }
}

pub(super) fn lock_fault_info(mapping: &VmMapping, address: Vaddr) -> Result<PageFaultInfo> {
    if !mapping.perms().intersects(VmPerms::ALL_PERMS) {
        return_errno!(Errno::ENOMEM);
    }
    #[cfg(target_arch = "x86_64")]
    if mapping.is_shadow_stack() && mapping.perms().contains(VmPerms::WRITE) {
        return Ok(PageFaultInfo::new(address, VmPerms::READ | VmPerms::WRITE)
            .shadow_stack()
            .force());
    }
    let required = if mapping.perms().contains(VmPerms::WRITE) && mapping.is_cow() {
        VmPerms::WRITE
    } else {
        VmPerms::READ
    };
    Ok(PageFaultInfo::new(address, required).force())
}
