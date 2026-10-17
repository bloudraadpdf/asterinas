// SPDX-License-Identifier: MPL-2.0

use super::SyscallReturn;
use crate::prelude::*;

pub(super) fn sys_setsid(ctx: &Context) -> Result<SyscallReturn> {
    ctx.process.to_new_session()?;
    let sid = ctx.process.sid_in(ctx.process.pid_namespace());

    Ok(SyscallReturn::Return(sid as _))
}
