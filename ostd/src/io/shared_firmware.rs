// SPDX-License-Identifier: MPL-2.0

//! Read-only access to reserved x86 firmware memory shared with a hypervisor.

use core::ops::Range;

use align_ext::AlignExt;

use super::IoMem;
use crate::{
    Error, Result,
    boot::{
        boot_info,
        memory_region::{MemoryRegion, MemoryRegionType},
    },
    mm::{CachePolicy, PAGE_SIZE, PageFlags, PodOnce, VmIo, VmIoOnce},
};

/// Reserved firmware bytes, with no writable mapping exposed to the kernel.
#[derive(Clone, Debug)]
pub struct SharedFirmwareMemory(IoMem);

impl SharedFirmwareMemory {
    /// Maps bytes from the reserved firmware window below the ACPI RSDP.
    /// Rejects a page that overlaps any region other than reserved firmware.
    pub fn acquire(range: Range<usize>) -> Result<Self> {
        validate_range(&boot_info().memory_regions, &range)?;
        #[cfg(feature = "cvm_guest")]
        crate::arch::if_tdx_enabled!({
            return Err(Error::AccessDenied);
        });

        // SAFETY:
        // 1. `boot_info` is available only after activation of the kernel page table.
        // 2. Every mapped page is reserved and overlaps no kernel, module or allocatable RAM.
        // 3. The mapping is read-only, uses the firmware RAM's writeback cache policy and
        //    exposes no operation that can modify firmware or kernel memory.
        let memory = unsafe { IoMem::new(range, PageFlags::R, CachePolicy::Writeback) };
        Ok(Self(memory))
    }

    /// Reads one aligned value without tearing.
    pub fn read_once<T: PodOnce>(&self, offset: usize) -> Result<T> {
        self.0.read_once(offset)
    }

    /// Copies bytes from shared firmware memory without blocking.
    pub fn read_bytes(&self, offset: usize, bytes: &mut [u8]) -> Result<()> {
        self.0.read_bytes(offset, bytes)
    }
}

fn validate_range(regions: &[MemoryRegion], range: &Range<usize>) -> Result<()> {
    const FIRMWARE_START: usize = 0x9fc00;
    const RSDP_START: usize = 0xe0000;

    if range.is_empty() {
        return Err(Error::InvalidArgs);
    }
    if range.start < FIRMWARE_START || range.end > RSDP_START {
        return Err(Error::AccessDenied);
    }
    let pages = range.start.align_down(PAGE_SIZE)..range.end.align_up(PAGE_SIZE);
    let covered = regions.iter().any(|region| {
        region.typ() == MemoryRegionType::Reserved
            && region.base() <= pages.start
            && region.end() >= pages.end
    });
    let overlaps_other = regions.iter().any(|region| {
        region.typ() != MemoryRegionType::Reserved
            && region.base() < pages.end
            && pages.start < region.end()
    });
    if !covered || overlaps_other {
        return Err(Error::AccessDenied);
    }
    Ok(())
}

#[cfg(ktest)]
mod tests {
    use super::*;
    use crate::prelude::ktest;

    #[ktest]
    fn firmware_mapping_rejects_live_memory_and_partial_pages() {
        let reserved = MemoryRegion::new(0x9f000, 0x41000, MemoryRegionType::Reserved);
        assert_eq!(validate_range(&[reserved], &(0xde000..0xdf000)), Ok(()));
        assert_eq!(validate_range(&[reserved], &(0xdfff0..0xe0000)), Ok(()));
        for kind in [
            MemoryRegionType::BadMemory,
            MemoryRegionType::NonVolatileSleep,
            MemoryRegionType::Usable,
            MemoryRegionType::Kernel,
            MemoryRegionType::Module,
            MemoryRegionType::Reclaimable,
            MemoryRegionType::Framebuffer,
            MemoryRegionType::Unknown,
        ] {
            let live = MemoryRegion::new(0xde000, PAGE_SIZE, kind);
            assert_eq!(
                validate_range(&[reserved, live], &(0xde000..0xde070)),
                Err(Error::AccessDenied)
            );
        }
        let partial = MemoryRegion::new(0xdfff0, 16, MemoryRegionType::Reserved);
        assert_eq!(
            validate_range(&[partial], &(0xdfff0..0xe0000)),
            Err(Error::AccessDenied)
        );
        assert_eq!(
            validate_range(&[], &(0xde000..0xdf000)),
            Err(Error::AccessDenied)
        );
        assert_eq!(
            validate_range(&[reserved], &(0xde000..0xde000)),
            Err(Error::InvalidArgs)
        );
        assert_eq!(
            validate_range(&[reserved], &(0xe0000..0xe1000)),
            Err(Error::AccessDenied)
        );
    }
}
