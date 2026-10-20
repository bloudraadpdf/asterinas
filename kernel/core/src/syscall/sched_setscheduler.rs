// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::{
    SyscallReturn,
    sched_getattr::{LinuxSchedAttr, update_sched_policy_with},
};
use crate::{prelude::*, thread::Tid};

pub(super) fn sys_sched_setscheduler(
    tid: Tid,
    policy: i32,
    addr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if addr == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid user space address");
    }

    let prio = ctx.user_space().read_val(addr)?;

    let attr = LinuxSchedAttr {
        sched_policy: policy as u32,
        sched_priority: prio,
        ..Default::default()
    };

    let policy = attr.try_into()?;
    update_sched_policy_with(tid, ctx, |_| Ok(policy))?;

    Ok(SyscallReturn::Return(0))
}
