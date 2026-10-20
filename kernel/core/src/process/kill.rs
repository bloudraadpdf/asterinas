// SPDX-License-Identifier: MPL-2.0

use super::{
    Pgid, Pid, Process, pid_table,
    posix_thread::AsPosixThread,
    signal::{
        c_types::siginfo_t,
        constants::SIGCONT,
        sig_num::SigNum,
        signals::{Signal, raw::RawSignal},
    },
};
use crate::{
    prelude::*,
    process::{credentials::capabilities::CapSet, posix_thread::PosixThread},
    security::lsm::hooks as lsm_hooks,
    thread::{Thread, Tid},
};

/// Sends a process-directed signal to the process containing the target PID or TID.
///
/// Checks the sender's credentials against the addressed thread's credentials.
///
/// If `signal` is `None`, this method will only check permission without sending
/// any signal.
pub(crate) fn kill(pid: Pid, signal: Option<Box<dyn Signal>>, ctx: &Context) -> Result<()> {
    // Fast path: If the signal is sent to self, we can skip most checks.
    if pid == ctx.process.pid() {
        let Some(signal) = signal else {
            return Ok(());
        };

        if !ctx.posix_thread.has_signal_blocked(signal.num()) {
            // Killing the current thread does not raise any permission issues.
            ctx.posix_thread.enqueue_signal(signal);
            return Ok(());
        }

        return kill_process(ctx.process.as_ref(), Some(signal), ctx);
    }

    let thread = signal_thread(pid)?;
    let receiver = thread.as_posix_thread().unwrap();
    let process = receiver
        .weak_process()
        .upgrade()
        .ok_or_else(|| Error::new(Errno::ESRCH))?;
    if process.pid_in(ctx.process.pid_namespace()) == 0 {
        return_errno_with_message!(
            Errno::ESRCH,
            "the target process is outside the PID namespace"
        );
    }

    // Linux checks the addressed task's credentials even for process-directed signals.
    // Reference: <https://elixir.bootlin.com/linux/v6.17/source/kernel/signal.c#L1449>.
    check_signal_perm(receiver, ctx, signal.as_ref().map(|signal| signal.num()))?;
    if let Some(signal) = signal {
        process.enqueue_signal(signal);
    }

    Ok(())
}

/// Sends a signal to all processes in a group, using the current process
/// as the sender.
///
/// The credentials of the current process will be checked to determine
/// if it is authorized to send the signal to the target group.
///
/// If `signal` is `None`, this method will only check permission without sending
/// any signal.
pub(crate) fn kill_group<S: Signal + Clone>(
    pgid: Pgid,
    signal: Option<S>,
    ctx: &Context,
) -> Result<()> {
    let process_group = pid_table::pid_table_mut()
        .get_process_group(&pgid)
        .ok_or_else(|| Error::with_message(Errno::ESRCH, "the target group does not exist"))?;

    let mut result = Ok(());

    let inner = process_group.lock();
    for process in inner.iter() {
        if process.pid_in(ctx.process.pid_namespace()) == 0 {
            continue;
        }
        let res = kill_process(
            &process,
            signal.clone().map(|s| Box::new(s) as Box<dyn Signal>),
            ctx,
        );
        if res.is_err_and(|err| err.error() != Errno::EPERM) {
            result = res;
        }
    }

    result
}

/// Sends a signal to a target thread, using the current process
/// as the sender.
///
/// Checks the target thread belongs to the target process if `tgid` is provided.
///
/// If `signal` is `None`, this method will only check permission without sending
/// any signal.
pub(crate) fn tgkill(
    tid: Tid,
    tgid: Option<Pid>,
    signal: Option<Box<dyn Signal>>,
    ctx: &Context,
) -> Result<()> {
    let thread = checked_thread(tid, tgid, signal.as_ref().map(|signal| signal.num()), ctx)?;
    let target_posix_thread = thread.as_posix_thread().unwrap();

    if thread.is_exited() {
        return Ok(());
    }

    if let Some(signal) = signal {
        // We've checked the permission issues above.
        // FIXME: We should take some lock while checking the permission to avoid race conditions.
        target_posix_thread.enqueue_signal(signal);
    }

    Ok(())
}

fn signal_thread(tid: Tid) -> Result<Arc<Thread>> {
    pid_table::pid_table_mut()
        .get_thread(tid)
        .ok_or_else(|| Error::with_message(Errno::ESRCH, "the target thread does not exist"))
}

fn checked_thread(
    tid: Tid,
    tgid: Option<Pid>,
    signum: Option<SigNum>,
    ctx: &Context,
) -> Result<Arc<Thread>> {
    let thread = signal_thread(tid)?;
    let target_posix_thread = thread.as_posix_thread().unwrap();
    if target_posix_thread
        .process()
        .pid_in(ctx.process.pid_namespace())
        == 0
    {
        return_errno_with_message!(
            Errno::ESRCH,
            "the target thread is outside the PID namespace"
        );
    }

    if let Some(tgid) = tgid {
        // Check the TGID
        let pid = target_posix_thread.process().pid();
        if pid != tgid {
            return_errno_with_message!(
                Errno::ESRCH,
                "the combination of the TGID and the TID is not valid"
            );
        }
    }

    // Check permission
    check_signal_perm(target_posix_thread, ctx, signum)?;

    Ok(thread)
}

pub(crate) enum QueuedSignalTarget {
    Process(Pid),
    Thread { tid: Tid, tgid: Pid },
}

fn queued_payload(info: siginfo_t, ctx: &Context) -> Result<Option<RawSignal>> {
    let number = u8::try_from(info.si_signo).map_err(|_| Error::new(Errno::EINVAL))?;
    if number == 0 {
        return Ok(None);
    }
    SigNum::try_from(number)?;
    Ok(Some(RawSignal::from_user(
        info,
        ctx.process.pid_namespace().clone(),
    )))
}

pub(crate) fn queue_signal(
    target: QueuedSignalTarget,
    info: siginfo_t,
    ctx: &Context,
) -> Result<()> {
    let (tid, group) = match &target {
        QueuedSignalTarget::Process(pid) => (*pid, None),
        QueuedSignalTarget::Thread { tid, tgid } => (*tid, Some(*tgid)),
    };
    let thread = signal_thread(tid)?;
    let receiver = thread.as_posix_thread().unwrap();
    let process = receiver
        .weak_process()
        .upgrade()
        .ok_or_else(|| Error::new(Errno::ESRCH))?;
    if group.is_some_and(|group| process.pid() != group) {
        return_errno!(Errno::ESRCH);
    }
    let signal = queued_payload(info, ctx)?;
    if process.pid_in(ctx.process.pid_namespace()) == 0 {
        return_errno!(Errno::ESRCH);
    }
    check_signal_perm(receiver, ctx, signal.as_ref().map(|signal| signal.num()))?;
    if let Some(signal) = signal {
        match target {
            QueuedSignalTarget::Process(_) => process.enqueue_queued_signal(receiver, signal)?,
            QueuedSignalTarget::Thread { .. } if !thread.is_exited() => {
                receiver.enqueue_queued_signal(signal)?
            }
            QueuedSignalTarget::Thread { .. } => {}
        }
    }
    Ok(())
}

/// Sends a signal to all processes except current process and init process, using
/// the current process as the sender.
///
/// The credentials of the current process will be checked to determine
/// if it is authorized to send the signal to the target group.
pub(crate) fn kill_all<S: Signal + Clone>(signal: Option<S>, ctx: &Context) -> Result<()> {
    let mut result = Ok(());

    for process in pid_table::pid_table_mut().iter_processes() {
        if Arc::ptr_eq(&ctx.process, &process) || process.pid_in(ctx.process.pid_namespace()) <= 1 {
            continue;
        }

        let res = kill_process(
            &process,
            signal.clone().map(|s| Box::new(s) as Box<dyn Signal>),
            ctx,
        );
        if res.is_err_and(|err| err.error() != Errno::EPERM) {
            result = res;
        }
    }

    result
}

fn kill_process(process: &Process, signal: Option<Box<dyn Signal>>, ctx: &Context) -> Result<()> {
    check_process_signal_perm(process, signal.as_ref().map(|signal| signal.num()), ctx)?;

    if let Some(signal) = signal {
        process.enqueue_signal(signal);
    }

    Ok(())
}

fn check_process_signal_perm(
    process: &Process,
    signum: Option<SigNum>,
    ctx: &Context,
) -> Result<()> {
    if process.pid_in(ctx.process.pid_namespace()) == 0 {
        return_errno_with_message!(
            Errno::ESRCH,
            "the target process is outside the PID namespace"
        );
    }
    let target_main_thread = process.main_thread();
    check_signal_perm(target_main_thread.as_posix_thread().unwrap(), ctx, signum)?;

    Ok(())
}

// Reference: <https://elixir.bootlin.com/linux/v6.17/source/kernel/signal.c#L799>.
fn check_signal_perm(target: &PosixThread, ctx: &Context, signum: Option<SigNum>) -> Result<()> {
    let target_process = target.process();

    if Arc::ptr_eq(&target_process, &ctx.process) {
        return Ok(());
    }

    let current_cred = ctx.posix_thread.credentials();
    let target_cred = target.credentials();
    if current_cred.euid() == target_cred.suid()
        || current_cred.euid() == target_cred.ruid()
        || current_cred.ruid() == target_cred.suid()
        || current_cred.ruid() == target_cred.ruid()
    {
        return Ok(());
    }

    if lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
        target_process.user_ns().lock().as_ref(),
        ctx.posix_thread,
        CapSet::KILL,
    ))
    .is_ok()
    {
        return Ok(());
    }

    if let Some(signum) = signum
        && signum == SIGCONT
    {
        let target_sid = target_process.sid();
        let current_sid = ctx.process.sid();
        if target_sid == current_sid {
            return Ok(());
        }
    }

    return_errno_with_message!(
        Errno::EPERM,
        "sending signal to the target process or thread is not allowed"
    )
}
