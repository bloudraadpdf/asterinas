// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    prelude::*,
    process::{
        Pid, QueuedSignalTarget, queue_signal,
        signal::{c_types::siginfo_t, constants::*, sig_num::SigNum},
    },
    thread::Tid,
};

fn known_layout(number: i32, code: i32) -> bool {
    if (-7..=0).contains(&code) || code == SI_ASYNCNL || code == SI_KERNEL {
        return true;
    }
    let number = u8::try_from(number)
        .ok()
        .and_then(|number| SigNum::try_from(number).ok());
    let limit = match number {
        Some(SIGILL) => 11,
        Some(SIGFPE) => 15,
        Some(SIGSEGV) => 10,
        Some(SIGBUS) => 5,
        Some(SIGSYS) => 2,
        _ => 6,
    };
    code > 0 && code <= limit
}

fn read_info(number: i32, address: Vaddr, ctx: &Context) -> Result<siginfo_t> {
    const KERNEL_INFO_SIZE: usize = 48;
    let mut bytes = [0; size_of::<siginfo_t>()];
    let user_space = ctx.user_space();
    user_space.read_bytes(address, &mut bytes[..KERNEL_INFO_SIZE])?;
    let mut info = siginfo_t::read_from_bytes(&bytes).map_err(|_| Error::new(Errno::EINVAL))?;
    info.si_signo = number;
    if !known_layout(number, info.si_code) {
        let expansion = address
            .checked_add(KERNEL_INFO_SIZE)
            .ok_or_else(|| Error::new(Errno::EFAULT))?;
        user_space.read_bytes(expansion, &mut bytes[KERNEL_INFO_SIZE..])?;
        if bytes[KERNEL_INFO_SIZE..].iter().any(|byte| *byte != 0) {
            return_errno!(Errno::E2BIG);
        }
    }
    Ok(info)
}

fn check_code(info: &siginfo_t, target_tid: Tid, ctx: &Context) -> Result<()> {
    if (info.si_code >= 0 || info.si_code == SI_TKILL)
        && target_tid != ctx.posix_thread.namespace_tid()
    {
        return_errno_with_message!(Errno::EPERM, "this signal code requires a self target");
    }
    Ok(())
}

pub(super) fn sys_rt_sigqueueinfo(
    pid: Pid,
    number: i32,
    address: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let info = read_info(number, address, ctx)?;
    check_code(&info, pid, ctx)?;
    if pid.cast_signed() <= 0 {
        return_errno!(Errno::ESRCH);
    }
    let pid = ctx
        .process
        .pid_namespace()
        .resolve(pid)
        .ok_or_else(|| Error::new(Errno::ESRCH))?;
    queue_signal(QueuedSignalTarget::Process(pid), info, ctx)?;
    Ok(SyscallReturn::Return(0))
}

pub(super) fn sys_rt_tgsigqueueinfo(
    tgid: Pid,
    tid: Tid,
    number: i32,
    address: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let info = read_info(number, address, ctx)?;
    if tgid.cast_signed() <= 0 || tid.cast_signed() <= 0 {
        return_errno!(Errno::EINVAL);
    }
    check_code(&info, tid, ctx)?;
    let namespace = ctx.process.pid_namespace();
    let tgid = namespace
        .resolve(tgid)
        .ok_or_else(|| Error::new(Errno::ESRCH))?;
    let tid = namespace
        .resolve(tid)
        .ok_or_else(|| Error::new(Errno::ESRCH))?;
    queue_signal(QueuedSignalTarget::Thread { tid, tgid }, info, ctx)?;
    Ok(SyscallReturn::Return(0))
}
