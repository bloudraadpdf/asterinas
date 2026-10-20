// SPDX-License-Identifier: MPL-2.0

#![short_vis_path::add(vmar)]

use align_ext::AlignExt;

use super::{Interval, RssDelta, VMAR_LOWEST_ADDR, Vmar, VmarInner};
use crate::{
    prelude::*,
    process::{Process, ResourceType},
    vm::perms::VmPerms,
};

impl Vmar {
    pub(crate) fn handle_page_fault(&self, page_fault_info: &PageFaultInfo) -> Result<()> {
        let inner = self.inner.read();

        let address = page_fault_info.address;
        if let Some(vm_mapping) = inner.vm_mappings.find_one(&address) {
            debug_assert!(vm_mapping.range().contains(&address));

            let mut rss_delta = RssDelta::new(self);
            return vm_mapping.handle_page_fault(&self.vm_space, page_fault_info, &mut rss_delta);
        }

        drop(inner);
        let mut inner = self.inner.write();
        let base = if let Some(mapping) = inner.vm_mappings.find_one(&address) {
            mapping.map_to_addr()
        } else {
            self.grow_stack(&mut inner, address)?
        };
        let mapping = inner.vm_mappings.find_one(&base).unwrap();
        let mut rss_delta = RssDelta::new(self);
        mapping.handle_page_fault(&self.vm_space, page_fault_info, &mut rss_delta)
    }

    fn grow_stack(&self, inner: &mut VmarInner, address: Vaddr) -> Result<Vaddr> {
        let start = address.align_down(PAGE_SIZE);
        if start < VMAR_LOWEST_ADDR {
            return_errno!(Errno::EFAULT);
        }
        let mapping = inner
            .vm_mappings
            .find_next(&start)
            .ok_or_else(|| Error::new(Errno::EFAULT))?;
        if !mapping.can_grow_down_to(start) {
            return_errno!(Errno::EFAULT);
        }
        let base = mapping.map_to_addr();
        let extra = base - start;
        if let Some(process) = Process::current() {
            let limit = process
                .resource_limits()
                .get_rlimit(ResourceType::RLIMIT_STACK)
                .get_cur();
            if (mapping.map_end() - start) as u64 > limit {
                return_errno!(Errno::EFAULT);
            }
        }
        inner
            .check_extra_size_fits_rlimit(extra)
            .map_err(|_| Error::new(Errno::EFAULT))?;
        if let Some(previous) = inner.vm_mappings.find_prev(&base)
            && (previous.map_end() > start
                || (!previous.grows_down()
                    && previous.perms().intersects(VmPerms::ALL_PERMS)
                    && start - previous.map_end() < 256 * PAGE_SIZE))
        {
            return_errno!(Errno::EFAULT);
        }
        let (mapping, mut removed) = inner.remove(&base).unwrap();
        let mut rmap = removed.remove(self, base);
        inner.insert_without_try_merge(self, mapping.grow_down(start), rmap.as_deref_mut());
        Ok(start)
    }
}

/// Page fault information converted from [`CpuException`].
///
/// `TryFrom<CpuException>` should be implemented for this struct.
/// If [`CpuException`] is a page fault, `try_from` should return `Ok(PageFaultInfo)`,
/// or `Err(())` (no error information) otherwise.
///
/// [`CpuException`]: ostd::arch::cpu::context::CpuException
#[derive(Debug)]
pub(crate) struct PageFaultInfo {
    /// The virtual address where a page fault occurred.
    pub(in vmar) address: Vaddr,

    /// The [`VmPerms`] required by the memory operation that causes page fault.
    /// For example, a "store" operation may require `VmPerms::WRITE`.
    pub(in vmar) required_perms: VmPerms,

    /// Whether this page fault is forced (e.g., manually triggered by `ptrace`).
    /// A forced page fault may bypass some permission checks.
    is_forced: bool,
}

impl PageFaultInfo {
    pub(crate) fn address(&self) -> Vaddr {
        self.address
    }

    /// Creates a new `PageFaultInfo`.
    pub(crate) fn new(address: Vaddr, required_perms: VmPerms) -> Self {
        Self {
            address,
            required_perms,
            is_forced: false,
        }
    }

    /// Returns whether this page fault is forced.
    pub(in vmar) fn is_forced(&self) -> bool {
        self.is_forced
    }

    /// Marks this page fault as forced.
    pub(super) fn force(mut self) -> Self {
        self.is_forced = true;
        self
    }
}
