// SPDX-License-Identifier: MPL-2.0

use super::SyscallReturn;
use crate::{prelude::*, vm::vmar::VMAR_CAP_ADDR};

pub(super) fn sys_mincore(
    address: Vaddr,
    length: usize,
    vector: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if !address.is_multiple_of(PAGE_SIZE) {
        return_errno_with_message!(Errno::EINVAL, "the address is not page aligned");
    }
    if VMAR_CAP_ADDR
        .checked_sub(address)
        .is_none_or(|remaining| length > remaining)
    {
        return_errno_with_message!(Errno::ENOMEM, "the range is outside userspace");
    }
    let pages = length / PAGE_SIZE + usize::from(!length.is_multiple_of(PAGE_SIZE));
    if VMAR_CAP_ADDR
        .checked_sub(vector)
        .is_none_or(|remaining| pages > remaining)
    {
        return_errno_with_message!(Errno::EFAULT, "the residency vector is outside userspace");
    }
    let user_space = ctx.user_space();
    let vmar = user_space.vmar();
    let mut buffer = [0u8; 256];
    let mut completed = 0;
    while completed < pages {
        let chunk = buffer.len().min(pages - completed);
        let count =
            vmar.query_residency(address + completed * PAGE_SIZE, &mut buffer[..chunk], ctx)?;
        let mut writer = user_space.writer(vector + completed, count)?;
        writer.write_fallible(&mut VmReader::from(&buffer[..count]).to_fallible())?;
        completed += count;
    }
    Ok(SyscallReturn::Return(0))
}
