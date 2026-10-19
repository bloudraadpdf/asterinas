// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    ipc::{
        IpcAccess, IpcControlCmd, IpcId,
        semaphore::system_v::{sem::Semaphore, sem_set::SemidDs},
    },
    prelude::*,
    process::Pid,
};

pub(super) fn sys_semctl(
    semid: i32,
    semnum: i32,
    op: i32,
    arg: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let Ok(semid) = IpcId::try_from(semid.cast_unsigned()) else {
        return_errno_with_message!(Errno::EINVAL, "non-positive semaphore IDs are invalid");
    };
    let cmd = IpcControlCmd::try_from(op & !0x100)?;

    debug!(
        "semctl: semid = {:?}, semnum = {}, cmd = {:?}, arg = {:x}",
        semid, semnum, cmd, arg
    );

    let ns_proxy = ctx.thread_local.borrow_ns_proxy();
    let ipc_ns = ns_proxy.unwrap().ipc_ns();

    match cmd {
        IpcControlCmd::IPC_RMID => ipc_ns.remove_sem_set(semid, ctx)?,
        IpcControlCmd::IPC_SET => {
            let info = ctx.user_space().read_val::<SemidDs>(arg)?;
            ipc_ns.set_sem_permission(semid, &info.sem_perm, ctx)?;
        }
        IpcControlCmd::IPC_STAT => {
            ipc_ns.with_sem_set(semid, IpcAccess::READ, ctx, |sem_set| {
                let semid_ds = sem_set.semid_ds();
                Ok(ctx.user_space().write_val(arg as Vaddr, &semid_ds)?)
            })?;
        }
        IpcControlCmd::SEM_GETALL => {
            ipc_ns.with_sem_set(semid, IpcAccess::READ, ctx, |sem_set| {
                ctx.user_space().write_slice(arg, &sem_set.values()?)?;
                Ok(())
            })?;
        }
        IpcControlCmd::SEM_SETALL => {
            ipc_ns.with_sem_set(semid, IpcAccess::WRITE, ctx, |sem_set| {
                let count = sem_set.num_sems();
                let mut values = Vec::new();
                values
                    .try_reserve_exact(count)
                    .map_err(|_| Error::new(Errno::ENOMEM))?;
                values.resize(count, 0u16);
                ctx.user_space().read_slice(arg, &mut values)?;
                sem_set.set_values(&values, ctx.process.pid_identity().clone())
            })?;
        }
        IpcControlCmd::SEM_GETPID => {
            fn sem_pid(sem: &Semaphore) -> Pid {
                sem.latest_modified_pid(current!().pid_namespace())
            }
            let pid: Pid = ipc_ns.with_sem_set(semid, IpcAccess::READ, ctx, |sem_set| {
                sem_set.get(semnum as usize, sem_pid)
            })?;

            return Ok(SyscallReturn::Return(pid as isize));
        }
        IpcControlCmd::SEM_GETVAL => {
            fn sem_val(sem: &Semaphore) -> i32 {
                sem.val()
            }
            let val: i32 = ipc_ns.with_sem_set(semid, IpcAccess::READ, ctx, |sem_set| {
                sem_set.get(semnum as usize, sem_val)
            })?;

            return Ok(SyscallReturn::Return(val as isize));
        }
        IpcControlCmd::SEM_GETNCNT => {
            let cnt: usize = ipc_ns.with_sem_set(semid, IpcAccess::READ, ctx, |sem_set| {
                sem_set.pending_decrease_count(semnum as usize)
            })?;

            return Ok(SyscallReturn::Return(cnt as isize));
        }
        IpcControlCmd::SEM_GETZCNT => {
            let cnt: usize = ipc_ns.with_sem_set(semid, IpcAccess::READ, ctx, |sem_set| {
                sem_set.pending_zero_count(semnum as usize)
            })?;

            return Ok(SyscallReturn::Return(cnt as isize));
        }
        IpcControlCmd::SEM_SETVAL => {
            // In `SEM_SETVAL`, the argument is parsed as an `i32`.
            let val = arg as i32;

            ipc_ns.with_sem_set(semid, IpcAccess::WRITE, ctx, |sem_set| {
                sem_set.setval(semnum as usize, val, ctx.process.pid_identity().clone())
            })?;
        }
    }

    Ok(SyscallReturn::Return(0))
}
