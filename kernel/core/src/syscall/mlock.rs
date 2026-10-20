// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use align_ext::AlignExt;

use super::SyscallReturn;
use crate::{
    prelude::*,
    vm::vmar::{VMAR_CAP_ADDR, current_lock_limit},
};

pub(super) fn sys_mlock(address: Vaddr, length: usize, ctx: &Context) -> Result<SyscallReturn> {
    if current_lock_limit() == 0 {
        return_errno!(Errno::EPERM);
    }
    let range = lock_range(address, length)?;
    ctx.user_space().vmar().set_memory_lock(range, true)?;
    Ok(SyscallReturn::Return(0))
}

pub(super) fn sys_munlock(address: Vaddr, length: usize, ctx: &Context) -> Result<SyscallReturn> {
    let range = lock_range(address, length)?;
    ctx.user_space().vmar().set_memory_lock(range, false)?;
    Ok(SyscallReturn::Return(0))
}

fn lock_range(address: Vaddr, length: usize) -> Result<Range<Vaddr>> {
    let start = address.align_down(PAGE_SIZE);
    let end = address
        .checked_add(length)
        .and_then(|end| end.checked_add(PAGE_SIZE - 1))
        .ok_or_else(|| Error::new(Errno::EINVAL))?
        .align_down(PAGE_SIZE);
    if end > VMAR_CAP_ADDR {
        return_errno!(Errno::ENOMEM);
    }
    Ok(start..end)
}
