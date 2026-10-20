// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::{
    SyscallReturn, sched_get_priority_max::static_to_rt, sched_getattr::update_sched_policy_with,
};
use crate::{prelude::*, sched::SchedPolicy, thread::Tid};

pub(super) fn sys_sched_setparam(tid: Tid, addr: Vaddr, ctx: &Context) -> Result<SyscallReturn> {
    if addr == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid user space address");
    }

    let prio: i32 = ctx.user_space().read_val(addr)?;

    let update = |mut policy: SchedPolicy| {
        match &mut policy {
            SchedPolicy::RealTime { rt_prio, .. } => {
                *rt_prio = static_to_rt(prio as u32)?;
            }
            _ if prio != 0 => {
                return_errno_with_message!(Errno::EINVAL, "invalid scheduling priority")
            }
            _ => {}
        }
        Ok(policy)
    };
    update_sched_policy_with(tid, ctx, update)?;

    Ok(SyscallReturn::Return(0))
}
