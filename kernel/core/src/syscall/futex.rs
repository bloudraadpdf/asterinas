// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use ostd::mm::VmIo;

use crate::{
    prelude::*,
    process::posix_thread::futex::{
        FutexFlags, FutexOp, FutexVisibility, futex_op_and_flags_from_u32, futex_requeue,
        futex_wait, futex_wait_bitset, futex_wake, futex_wake_bitset, futex_wake_op, pi,
    },
    syscall::SyscallReturn,
    time::{
        clocks::{MonotonicClock, RealTimeClock},
        timer::Timeout,
        timespec_t,
        wait::ManagedTimeout,
    },
};

pub(super) fn sys_futex(
    futex_addr: Vaddr,
    futex_op: i32,
    futex_val: u32,
    utime_addr: Vaddr,
    futex_new_addr: Vaddr,
    bitset: u32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let (futex_op, futex_flags) = futex_op_and_flags_from_u32(futex_op as _)?;
    debug!(
        "futex_op = {:?}, futex_flags = {:?}, futex_addr = 0x{:x}, futex_val = 0x{:x}",
        futex_op, futex_flags, futex_addr, futex_val
    );

    let get_futex_timeout = |timeout_addr: Vaddr| -> Result<Option<ManagedTimeout<'static>>> {
        if timeout_addr == 0 {
            return Ok(None);
        }

        let timeout = {
            let time_spec: timespec_t = ctx.user_space().read_val(timeout_addr)?;
            Duration::try_from(time_spec)?
        };

        let is_real_time = futex_flags.contains(FutexFlags::FUTEX_CLOCK_REALTIME)
            || futex_op == FutexOp::FUTEX_LOCK_PI;
        let timeout = {
            // For `FUTEX_WAIT`, `timeout` is interpreted as a relative value. This differs from
            // other futex operations, where `timeout` is interpreted as an absolute value.
            // Reference: <https://www.man7.org/linux/man-pages/man2/FUTEX_WAIT.2const.html>
            if futex_op == FutexOp::FUTEX_WAIT {
                Timeout::After(timeout)
            } else {
                Timeout::When(timeout)
            }
        };

        let timer_manager = if is_real_time {
            debug!("futex timeout = {:?}, clock = CLOCK_REALTIME", timeout);
            RealTimeClock::timer_manager()
        } else {
            debug!("futex timeout = {:?}, clock = CLOCK_MONOTONIC", timeout);
            MonotonicClock::timer_manager()
        };

        Ok(Some(ManagedTimeout::new_with_manager(
            timeout,
            timer_manager,
        )))
    };

    let visibility = FutexVisibility::from(futex_flags);
    let is_pi = matches!(
        futex_op,
        FutexOp::FUTEX_LOCK_PI
            | FutexOp::FUTEX_LOCK_PI2
            | FutexOp::FUTEX_TRYLOCK_PI
            | FutexOp::FUTEX_UNLOCK_PI
    );
    let timeout = if matches!(
        futex_op,
        FutexOp::FUTEX_WAIT
            | FutexOp::FUTEX_WAIT_BITSET
            | FutexOp::FUTEX_LOCK_PI
            | FutexOp::FUTEX_LOCK_PI2
    ) {
        get_futex_timeout(utime_addr)?
    } else {
        None
    };
    if futex_flags.contains(FutexFlags::FUTEX_CLOCK_REALTIME)
        && !matches!(
            futex_op,
            FutexOp::FUTEX_WAIT_BITSET | FutexOp::FUTEX_LOCK_PI2
        )
    {
        return_errno_with_message!(
            Errno::ENOSYS,
            "this futex command cannot use CLOCK_REALTIME"
        );
    }
    let res = match futex_op {
        FutexOp::FUTEX_WAIT => {
            futex_wait(futex_addr as _, futex_val as _, timeout, ctx, visibility).map(|_| 0)
        }
        FutexOp::FUTEX_WAIT_BITSET => futex_wait_bitset(
            futex_addr as _,
            futex_val as _,
            timeout,
            bitset as _,
            ctx,
            visibility,
        )
        .map(|_| 0),
        FutexOp::FUTEX_WAKE => {
            let max_count = futex_val_to_max_count(futex_val);
            futex_wake(futex_addr as _, max_count, visibility)
        }
        FutexOp::FUTEX_WAKE_BITSET => {
            let max_count = futex_val_to_max_count(futex_val);
            futex_wake_bitset(futex_addr as _, max_count, bitset as _, visibility)
        }
        FutexOp::FUTEX_REQUEUE => {
            let max_nwakes = futex_val as i32;
            let max_nrequeues = utime_addr as i32;
            if max_nwakes < 0 || max_nrequeues < 0 {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "futex requeue counts cannot be negative"
                );
            }
            let max_nwakes = max_nwakes as usize;
            let max_nrequeues = max_nrequeues as usize;
            futex_requeue(
                futex_addr as _,
                max_nwakes,
                max_nrequeues,
                futex_new_addr as _,
                ctx,
                visibility,
            )
        }
        FutexOp::FUTEX_WAKE_OP => {
            let futex_val_2 = utime_addr as u32;
            futex_wake_op(
                futex_addr,
                futex_new_addr,
                futex_val_to_max_count(futex_val),
                futex_val_to_max_count(futex_val_2),
                bitset,
                ctx,
                visibility,
            )
        }
        FutexOp::FUTEX_LOCK_PI | FutexOp::FUTEX_LOCK_PI2 => {
            pi::lock_pi(futex_addr, pi::PiLockMode::Wait, timeout, visibility, ctx).map(|_| 0)
        }
        FutexOp::FUTEX_TRYLOCK_PI => {
            pi::lock_pi(futex_addr, pi::PiLockMode::Try, None, visibility, ctx).map(|_| 0)
        }
        FutexOp::FUTEX_UNLOCK_PI => pi::unlock_pi(futex_addr, visibility, ctx).map(|_| 0),
        _ => {
            warn!("futex op = {:?}", futex_op);
            return_errno_with_message!(Errno::ENOSYS, "unsupported futex op");
        }
    }
    .map_err(|err| match err.error() {
        Errno::ETIME => Error::new(Errno::ETIMEDOUT),
        Errno::EINTR if is_pi => Error::new(Errno::ERESTARTNOINTR),
        Errno::EINTR => Error::new(Errno::ERESTARTSYS),
        _ => err,
    })?;

    debug!("futex returns, tid = {}", ctx.posix_thread.tid());
    Ok(SyscallReturn::Return(res as _))
}

fn futex_val_to_max_count(futex_val: u32) -> usize {
    // Linux wakes one waiter even if `futex_val` is 0 or negative. To be
    // consistent with Linux, we set `max_count` to 1 if it is 0 or negative.
    // Reference: <https://elixir.bootlin.com/linux/v7.2/source/kernel/futex/waitwake.c#L219>
    (futex_val as i32).max(1) as usize
}
