// SPDX-License-Identifier: MPL-2.0

use core::cell::Cell;

use ostd::{arch::cpu::context::UserContext, cpu::CpuId, mm::VmIo, user::UserContextApi};

use crate::{prelude::*, vm::vmar::VMAR_CAP_ADDR};

#[derive(Clone, Copy)]
pub(crate) struct Registration {
    address: Vaddr,
    length: u32,
    signature: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod)]
struct CriticalSection {
    version: u32,
    flags: u32,
    start: u64,
    length: u64,
    abort: u64,
}

pub(crate) struct RseqState {
    registration: Cell<Option<Registration>>,
    pending: Cell<bool>,
}

impl RseqState {
    pub(super) const fn new() -> Self {
        Self {
            registration: Cell::new(None),
            pending: Cell::new(false),
        }
    }

    pub(crate) fn registration(&self) -> Option<Registration> {
        self.registration.get()
    }

    pub(super) fn inherit(&self, registration: Option<Registration>) {
        self.registration.set(registration);
        self.pending.set(registration.is_some());
    }

    pub(crate) fn reset(&self) {
        self.inherit(None);
    }

    pub(crate) fn notify(&self) {
        if self.registration.get().is_some() {
            self.pending.set(true);
        }
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.get()
    }

    pub(crate) fn register(
        &self,
        address: Vaddr,
        length: u32,
        flags: u32,
        signature: u32,
        ctx: &Context,
    ) -> Result<()> {
        if flags & !1 != 0 {
            return_errno!(Errno::EINVAL);
        }
        if flags == 1 {
            let old = self
                .registration
                .get()
                .filter(|old| old.address == address && old.length == length)
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            if old.signature != signature {
                return_errno!(Errno::EPERM);
            }
            let space = ctx.user_space();
            space.write_val(address, &0u32)?;
            space.write_val(address + 4, &u32::MAX)?;
            space.write_val(address + 20, &0u32)?;
            space.write_val(address + 24, &0u32)?;
            self.reset();
            return Ok(());
        }
        if let Some(old) = self.registration.get() {
            if old.address != address || old.length != length {
                return_errno!(Errno::EINVAL);
            }
            if old.signature != signature {
                return_errno!(Errno::EPERM);
            }
            return_errno!(Errno::EBUSY);
        }
        if length < 32 || !address.is_multiple_of(32) {
            return_errno!(Errno::EINVAL);
        }
        if address == 0
            || VMAR_CAP_ADDR
                .checked_sub(address)
                .is_none_or(|available| available < length as usize)
        {
            return_errno!(Errno::EFAULT);
        }
        let space = ctx.user_space();
        space.write_val(address + 8, &0u64)?;
        space.write_val(address + 16, &0u32)?;
        self.registration.set(Some(Registration {
            address,
            length,
            signature,
        }));
        self.pending.set(true);
        Ok(())
    }

    pub(crate) fn prepare(&self, user_ctx: &mut UserContext, ctx: &Context) -> Result<()> {
        if !self.pending.replace(false) {
            return Ok(());
        }
        let Some(registration) = self.registration.get() else {
            return Ok(());
        };
        self.fixup(registration, user_ctx, ctx)?;
        let cpu: u32 = CpuId::current_racy().into();
        let space = ctx.user_space();
        space.write_val(registration.address, &cpu)?;
        space.write_val(registration.address + 4, &cpu)?;
        space.write_val(registration.address + 20, &0u32)?;
        space.write_val(registration.address + 24, &cpu)?;
        Ok(())
    }

    pub(crate) fn before_signal(&self, user_ctx: &mut UserContext, ctx: &Context) -> Result<()> {
        self.notify();
        self.prepare(user_ctx, ctx)
    }

    fn fixup(
        &self,
        registration: Registration,
        user_ctx: &mut UserContext,
        ctx: &Context,
    ) -> Result<()> {
        let space = ctx.user_space();
        let pointer = space.read_val::<u64>(registration.address + 8)?;
        if pointer == 0 {
            return Ok(());
        }
        if pointer >= VMAR_CAP_ADDR as u64 {
            return_errno!(Errno::EINVAL);
        }
        let section = space.read_val::<CriticalSection>(pointer as Vaddr)?;
        let end = section
            .start
            .checked_add(section.length)
            .filter(|end| *end < VMAR_CAP_ADDR as u64)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if section.version != 0
            || section.start >= VMAR_CAP_ADDR as u64
            || section.abort >= VMAR_CAP_ADDR as u64
            || (section.start..end).contains(&section.abort)
        {
            return_errno!(Errno::EINVAL);
        }
        let signature_address = section
            .abort
            .checked_sub(4)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        if space.read_val::<u32>(signature_address as Vaddr)? != registration.signature {
            return_errno!(Errno::EINVAL);
        }
        let in_section = (section.start..end).contains(&(user_ctx.instruction_pointer() as u64));
        if in_section
            && (section.flags != 0 || space.read_val::<u32>(registration.address + 16)? != 0)
        {
            return_errno!(Errno::EINVAL);
        }
        space.write_val(registration.address + 8, &0u64)?;
        if in_section {
            user_ctx.set_instruction_pointer(section.abort as Vaddr);
        }
        Ok(())
    }
}
