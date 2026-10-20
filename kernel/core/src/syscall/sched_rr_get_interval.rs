// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    prelude::*,
    process::pid_table,
    sched,
    thread::{Thread, Tid},
    time::timespec_t,
};

pub(super) fn sys_sched_rr_get_interval(
    tid: Tid,
    interval: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if tid.cast_signed() < 0 {
        return_errno_with_message!(Errno::EINVAL, "the thread ID is negative");
    }
    let thread = if tid == 0 {
        Thread::current()
    } else {
        pid_table::pid_table_mut().get_thread_in(tid, ctx.process.pid_namespace())
    }
    .ok_or_else(|| Error::new(Errno::ESRCH))?;
    let duration = sched::round_robin_interval(&thread);
    ctx.user_space()
        .write_val(interval, &timespec_t::from(duration))?;
    Ok(SyscallReturn::Return(0))
}
