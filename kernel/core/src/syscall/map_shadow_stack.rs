// SPDX-License-Identifier: MPL-2.0

use align_ext::AlignExt;
use ostd::arch::cpu::context::UserShadowStackContext;

use super::SyscallReturn;
use crate::prelude::*;

pub(super) fn sys_map_shadow_stack(
    address: Vaddr,
    size: usize,
    flags: u32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if !UserShadowStackContext::is_available() {
        return_errno!(Errno::EOPNOTSUPP);
    }
    if flags & !1 != 0 {
        return_errno!(Errno::EINVAL);
    }
    let token = flags & 1 != 0;
    if token && size < 8 {
        return_errno!(Errno::ENOSPC);
    }
    if address != 0 && address < 1usize << 32 {
        return_errno!(Errno::ERANGE);
    }
    let aligned = size
        .checked_add(PAGE_SIZE - 1)
        .ok_or_else(|| Error::new(Errno::EOVERFLOW))?
        .align_down(PAGE_SIZE);
    let user = ctx.user_space();
    let mapped = user.vmar().map_shadow_stack(address, aligned)?;
    if token {
        let top = mapped + size;
        if !top.is_multiple_of(8)
            || user
                .vmar()
                .write_shadow_stack_word(top - 8, top as u64 | 1)
                .is_err()
        {
            user.vmar().remove_mapping(mapped..mapped + aligned);
            return_errno!(Errno::EINVAL);
        }
    }
    Ok(SyscallReturn::Return(mapped as _))
}
