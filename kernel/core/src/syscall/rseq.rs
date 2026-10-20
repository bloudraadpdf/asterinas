// SPDX-License-Identifier: MPL-2.0

use super::SyscallReturn;
use crate::prelude::*;

pub(super) fn sys_rseq(
    address: Vaddr,
    length: u32,
    flags: u32,
    signature: u32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    ctx.thread_local
        .rseq()
        .register(address, length, flags, signature, ctx)?;
    Ok(SyscallReturn::Return(0))
}
