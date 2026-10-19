// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::Ordering;

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    prelude::*,
    process::{
        Pid, pid_table,
        posix_thread::{AsPosixThread, RobustListHead, alien_access::AlienAccessMode},
    },
};

pub(super) fn sys_set_robust_list(
    robust_list_head_ptr: Vaddr,
    len: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    debug!(
        "robust list head ptr: 0x{:x}, len = {}",
        robust_list_head_ptr, len
    );

    if len != size_of::<RobustListHead>() {
        return_errno_with_message!(
            Errno::EINVAL,
            "the length is not equal to the size of the robust list head"
        );
    }

    ctx.posix_thread
        .robust_list()
        .store(robust_list_head_ptr, Ordering::Relaxed);

    Ok(SyscallReturn::Return(0))
}

pub(super) fn sys_get_robust_list(
    pid: Pid,
    head_ptr: Vaddr,
    length_ptr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let head = if pid == 0 || pid == ctx.posix_thread.namespace_tid() {
        ctx.posix_thread.robust_list().load(Ordering::Relaxed)
    } else {
        let target = pid_table::pid_table_mut()
            .get_thread_in(pid, ctx.process.pid_namespace())
            .ok_or_else(|| Error::with_message(Errno::ESRCH, "the target thread does not exist"))?;
        let target = target.as_posix_thread().unwrap();
        target.check_alien_access_from(ctx.posix_thread, AlienAccessMode::READ_WITH_REAL_CREDS)?;
        target.robust_list().load(Ordering::Relaxed)
    };
    let user_space = ctx.user_space();
    user_space.write_val(length_ptr, &size_of::<RobustListHead>())?;
    user_space.write_val(head_ptr, &head)?;
    Ok(SyscallReturn::Return(0))
}
