// SPDX-License-Identifier: MPL-2.0

pub(crate) mod c_types;
pub(crate) mod constants;
mod pause;
mod pending;
pub(crate) mod pending_quota;
mod poll;
pub(crate) mod sig_action;
pub(crate) mod sig_disposition;
pub(crate) mod sig_mask;
pub(crate) mod sig_num;
pub(crate) mod sig_queues;
mod sig_stack;
pub(crate) mod signals;
#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_fpu;

use align_ext::AlignExt;
use c_types::{siginfo_t, ucontext_t};
use constants::{SI_KERNEL, SIGSEGV};
use ostd::{
    arch::cpu::context::{FpuContext, UserContext},
    mm::VmIo,
    user::UserContextApi,
};
pub(crate) use pause::{Pause, PauseReason, with_sigmask_changed};
pub(crate) use pending::{DequeuedSignal, HandlePendingSignal};
pub(crate) use poll::{PollAdaptor, Pollee, Poller};
pub use poll::{PollHandle, Pollable};
use sig_action::{SigAction, SigActionFlags, SigDefaultAction};
use sig_mask::SigMask;
use sig_num::SigNum;
pub(crate) use sig_stack::{SigStack, SigStackFlags, SigStackStatus};
use signals::{Signal, fault::FaultSignal};

use super::posix_thread::ThreadLocal;
use crate::{
    cpu::LinuxAbi,
    prelude::*,
    process::{
        TermStatus,
        posix_thread::{ContextPthreadAdminApi, do_exit_group, ptrace::PtraceStopResult},
        signal::{c_types::stack_t, constants::SIGKILL},
    },
};

pub(crate) trait SignalContext {
    /// Sets signal handler arguments.
    fn set_arguments(&mut self, sig_num: SigNum, siginfo_addr: usize, ucontext_addr: usize);
}

pub(crate) fn force_fault_signal(signal: FaultSignal, ctx: &Context) {
    let number = signal.num();
    let mask = ctx.posix_thread.sig_mask();
    {
        let dispositions = ctx.process.sig_dispositions().lock();
        let mut dispositions = dispositions.lock();
        if mask.contains(number) || matches!(dispositions.get(number), SigAction::Ign) {
            dispositions.set_default(number);
        }
    }
    ctx.set_sig_mask(mask - number);
    ctx.posix_thread.enqueue_signal(Box::new(signal));
}

fn restart_interrupted_syscall(user_ctx: &mut UserContext, original: usize) {
    #[cfg(target_arch = "x86_64")]
    const INSTRUCTION_LENGTH: usize = 2;
    #[cfg(any(
        target_arch = "riscv64",
        target_arch = "loongarch64",
        target_arch = "aarch64"
    ))]
    const INSTRUCTION_LENGTH: usize = 4;

    if let Some(address) = user_ctx
        .instruction_pointer()
        .checked_sub(INSTRUCTION_LENGTH)
    {
        user_ctx.set_syscall_ret(original);
        user_ctx.set_instruction_pointer(address);
    }
}

/// Handles a pending signal for the current process.
pub(crate) fn handle_pending_signal(user_ctx: &mut UserContext, ctx: &Context) {
    while matches!(
        handle_one_pending_signal(user_ctx, ctx),
        SignalProgress::ForcedFault
    ) {}
}

enum SignalProgress {
    Complete,
    ForcedFault,
}

fn handle_one_pending_signal(user_ctx: &mut UserContext, ctx: &Context) -> SignalProgress {
    // FIXME: This function may handle or suppress only one signal per trap, delaying
    // other pending unmasked signals until the next trap. Consider a looped scan.
    //
    // For details, see <https://github.com/asterinas/asterinas/pull/2984/#discussion_r3137275994>.
    let syscall_restart = ctx.thread_local.orig_syscall_ret().and_then(|original| {
        let result = user_ctx.syscall_ret();
        let restart = result == -(Errno::ERESTARTSYS as i32) as usize;
        let unconditional = result == -(Errno::ERESTARTNOINTR as i32) as usize;
        if !restart && !unconditional {
            return None;
        }
        user_ctx.set_syscall_ret(-(Errno::EINTR as i32) as usize);
        if unconditional {
            restart_interrupted_syscall(user_ctx, original);
            None
        } else {
            Some(original)
        }
    });

    let mut restore_sig_mask = ctx
        .thread_local
        .sig_mask_saved()
        .take()
        .map(|mask| RestoreSigMaskGuard { ctx, mask });

    let (dequeued, sig_action) = if let Some(dequeued_signal) = dequeue_pending_signal(ctx) {
        dequeued_signal
    } else {
        // Fast path: There is no signal mask to restore.
        if restore_sig_mask.is_none() {
            return SignalProgress::Complete;
        }
        // Restore the signal mask first.
        let _ = restore_sig_mask.take();

        // Try again with the new signal mask.
        if let Some(dequeued_signal) = dequeue_pending_signal(ctx) {
            dequeued_signal
        } else {
            return SignalProgress::Complete;
        }
    };

    let (signal, sig_action) = if dequeued.num() != SIGKILL {
        match ctx.posix_thread.ptrace_stop(dequeued, ctx, user_ctx) {
            PtraceStopResult::Continued(Some(dequeued)) => {
                // Note that this `dequeued` object outputted by `ptrace_stop`
                // might be different from the input `dequeued` object
                // because of tracer manipulation.
                // But we name both `dequeued` anyway.

                let sig_num = dequeued.num();
                if ctx.posix_thread.sig_mask().contains(sig_num) {
                    // Requeue the injected signal to the same signal queue as the
                    // dequeued stop signal (thread queue or process queue).
                    //
                    // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/kernel/signal.c#L2770>
                    requeue_signal(ctx, dequeued);
                    return SignalProgress::Complete;
                }

                (dequeued.unwrap(), get_sig_action(ctx, sig_num))
            }
            PtraceStopResult::Continued(None) => return SignalProgress::Complete,
            PtraceStopResult::Interrupted => {
                match dequeue_pending_signal(ctx) {
                    Some((dequeued, action)) if dequeued.num() == SIGKILL => {
                        (dequeued.unwrap(), action)
                    }
                    // SIGKILL was consumed by a sibling thread; this thread will be
                    // terminated as part of the process-wide exit.
                    _ => return SignalProgress::Complete,
                }
            }
            PtraceStopResult::NotTraced(dequeued) => (dequeued.unwrap().unwrap(), sig_action),
        }
    } else {
        (dequeued.unwrap(), sig_action)
    };

    let sig_num = signal.num();
    match sig_action {
        SigAction::Ign => {
            debug!("Ignore signal {:?}", sig_num);
        }
        SigAction::User {
            handler_addr,
            flags,
            restorer_addr,
            mask,
        } => {
            if flags.contains(SigActionFlags::SA_RESETHAND) {
                // In Linux, SA_RESETHAND corresponds to SA_ONESHOT,
                // which means the user handler will be executed only once and then reset to the default.
                // Reference: <https://elixir.bootlin.com/linux/v6.0.9/source/kernel/signal.c#L2761>.
                let sig_dispositions = ctx.process.sig_dispositions().lock();
                let mut sig_dispositions = sig_dispositions.lock();
                sig_dispositions.set_default(sig_num);
            }

            if let Some(orig_syscall_ret) = syscall_restart
                && flags.contains(SigActionFlags::SA_RESTART)
            {
                restart_interrupted_syscall(user_ctx, orig_syscall_ret);
            }

            if let Err(e) = handle_user_signal(
                ctx,
                sig_num,
                handler_addr,
                flags,
                restorer_addr,
                mask,
                restore_sig_mask.map(RestoreSigMaskGuard::into_mask),
                user_ctx,
                signal.to_info(ctx.process.pid_namespace()),
            ) {
                debug!("Failed to handle user signal: {:?}", e);
                if sig_num == SIGSEGV {
                    ctx.process
                        .sig_dispositions()
                        .lock()
                        .lock()
                        .set_default(SIGSEGV);
                }
                force_fault_signal(FaultSignal::new(SIGSEGV, SI_KERNEL, None), ctx);
                return SignalProgress::ForcedFault;
            }
        }
        SigAction::Dfl if !ctx.process.accepts_signal(signal.as_ref()) => {
            // From Linux man pages "kill(2)":
            // "The only signals that can be sent to process ID 1, the init process, are those for
            // which init has explicitly installed signal handlers."
        }
        SigAction::Dfl => {
            let sig_default_action = SigDefaultAction::from_signum(sig_num);
            debug!("sig_default_action = {:?}", sig_default_action);

            match sig_default_action {
                SigDefaultAction::Core | SigDefaultAction::Term => {
                    warn!(
                        "PID {}: terminating on signal {}",
                        ctx.process.pid(),
                        sig_num.sig_name()
                    );
                    // The signal terminates the current process. Therefore, we should exit here.
                    do_exit_group(TermStatus::Killed(sig_num), ctx, user_ctx);
                }
                SigDefaultAction::Ign => {}
                SigDefaultAction::Stop => ctx.process.stop(sig_num),
                SigDefaultAction::Cont => ctx.process.resume(),
            }
        }
    }
    SignalProgress::Complete
}

/// A guard that restores the signal mask on drop.
struct RestoreSigMaskGuard<'a> {
    ctx: &'a Context<'a>,
    mask: SigMask,
}

impl RestoreSigMaskGuard<'_> {
    /// Forgets the guard and returns the signal mask to restore.
    fn into_mask(self) -> SigMask {
        let mask = self.mask;
        // Assert that it's a `Copy` type. So we won't leak resources.
        let _ = self.mask;

        core::mem::forget(self);

        mask
    }
}

impl Drop for RestoreSigMaskGuard<'_> {
    fn drop(&mut self) {
        self.ctx.set_sig_mask(self.mask);
    }
}

fn dequeue_pending_signal(ctx: &Context) -> Option<(DequeuedSignal, SigAction)> {
    let posix_thread = ctx.posix_thread;

    let sig_dispositions = ctx.process.sig_dispositions().lock();
    let sig_dispositions = sig_dispositions.lock();

    let sig_mask = posix_thread.sig_mask();
    let (signal, sig_num, sig_action) = loop {
        let signal = ctx.dequeue_signal(&sig_mask)?;
        let sig_num = signal.num();
        let sig_action = sig_dispositions.get(sig_num);
        if sig_action.will_ignore(sig_num) && !posix_thread.is_traced() {
            continue;
        }

        break (signal, sig_num, sig_action);
    };

    debug!(
        "sig_num = {:?}, sig_name = {}, sig_action = {:#x?}",
        sig_num,
        sig_num.sig_name(),
        sig_action
    );

    Some((signal, sig_action))
}

/// Re-queue the ptrace-captured signal.
///
/// This appends to the tail of the origin queue,
/// which (a) loses same-number RT-signal send-order FIFO and
/// (b) allows newer lower-numbered standard signals queued
/// during the ptrace stop to preempt this signal.
/// This is intentionally consistent with Linux's behaviors.
fn requeue_signal(ctx: &Context, signal: DequeuedSignal) {
    match signal {
        DequeuedSignal::FromProcess(signal) => ctx.process.enqueue_signal(signal),
        DequeuedSignal::FromThread(signal) => ctx.posix_thread.enqueue_signal(signal),
    }
}

fn get_sig_action(ctx: &Context, sig_num: SigNum) -> SigAction {
    let sig_dispositions = ctx.process.sig_dispositions().lock();
    let sig_dispositions = sig_dispositions.lock();
    sig_dispositions.get(sig_num)
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn handle_user_signal(
    ctx: &Context,
    sig_num: SigNum,
    handler_addr: Vaddr,
    flags: SigActionFlags,
    restorer_addr: Vaddr,
    mut mask: SigMask,
    mask_to_restore: Option<SigMask>,
    user_ctx: &mut UserContext,
    sig_info: siginfo_t,
) -> Result<()> {
    ctx.thread_local.rseq().before_signal(user_ctx, ctx)?;
    debug!("sig_num = {:?}, signame = {}", sig_num, sig_num.sig_name());
    debug!("handler_addr = 0x{:x}", handler_addr);
    debug!("flags = {:?}", flags);
    debug!("restorer_addr = 0x{:x}", restorer_addr);
    debug!("mask = {:?}, mask_to_restore = {:?}", mask, mask_to_restore);

    if flags.contains_unsupported_flag() {
        warn!("Unsupported signal flags: {:?}", flags);
    }

    if !flags.contains(SigActionFlags::SA_NODEFER) {
        // Add the current signal to `mask`.
        mask += sig_num;
    }

    let old_mask = ctx.posix_thread.sig_mask();
    let mask_to_restore = mask_to_restore.unwrap_or(old_mask);

    let alternate_bounds = {
        let stack = ctx.thread_local.sig_stack().borrow();
        let selected =
            use_alternate_signal_stack(flags, ctx.thread_local, user_ctx.stack_pointer()).is_some();
        (selected || stack.contains(user_ctx.stack_pointer()))
            .then(|| stack.base()..stack.base() + stack.size())
    };

    // Set up the signal stack.
    let mut stack_pointer = if let Some(sp) =
        use_alternate_signal_stack(flags, ctx.thread_local, user_ctx.stack_pointer())
    {
        sp as u64
    } else {
        // Just use the user stack.
        let sp = user_ctx.stack_pointer() as u64;

        // Prevent corruption of the current stack. Architectures like as x86-64 have red zones
        // that can be accessed below the SP. Signal handlers must not write to these zones.
        // FIXME: This may not be necessary for all architectures.
        sp.checked_sub(128)
            .ok_or_else(|| Error::new(Errno::EFAULT))?
    };

    let user_space = ctx.user_space();

    // 1. Write `siginfo_t`.
    stack_pointer = alloc_aligned_in_user_stack(
        stack_pointer,
        size_of::<siginfo_t>(),
        align_of::<siginfo_t>(),
    )?;
    let siginfo_addr = stack_pointer;

    // 2. Write `ucontext_t`.

    // Save the current signal stack information.
    let uc_stack = stack_t::from(&*ctx.thread_local.sig_stack().borrow());

    let mut ucontext = ucontext_t {
        uc_stack,
        uc_sigmask: mask_to_restore.into(),
        ..Default::default()
    };

    // Save general-purpose registers.
    ucontext.uc_mcontext.copy_user_regs_from(user_ctx);

    let supp = ctx.thread_local.supp_user_context();
    let fpu_context = supp.fpu().get();
    #[cfg(target_arch = "x86_64")]
    let image = x86_fpu::encode(&fpu_context)?;
    #[cfg(target_arch = "x86_64")]
    let fpu_context_bytes = image.bytes.as_slice();
    #[cfg(not(target_arch = "x86_64"))]
    let fpu_context_bytes = fpu_context.as_bytes();

    cfg_select! {
        target_arch = "x86_64" => {
            // Align the FPU context address to the 64-byte boundary so that the
            // user program can use the XSAVE/XRSTOR instructions at that address,
            // if necessary.
            let fpu_context_addr =
                alloc_aligned_in_user_stack(stack_pointer, fpu_context_bytes.len(), 64)?;
            let ucontext_addr = alloc_aligned_in_user_stack(
                fpu_context_addr,
                size_of::<ucontext_t>(),
                align_of::<ucontext_t>(),
            )?;
            ucontext
                .uc_mcontext
                .set_fpu_context_addr(fpu_context_addr as _);

            ucontext.uc_flags = image.flags;
        }
        target_arch = "riscv64" => {
            // Reference:
            // <https://elixir.bootlin.com/linux/v6.17.5/source/arch/riscv/include/uapi/asm/ptrace.h#L94-L98>,
            // <https://elixir.bootlin.com/linux/v6.17.5/source/arch/riscv/include/uapi/asm/ptrace.h#L69-L77>.
            const FP_STATE_SIZE: usize =
                size_of::<ostd::arch::cpu::context::QFpuContext>() + 3 * size_of::<u32>();

            let ucontext_addr = alloc_aligned_in_user_stack(
                stack_pointer,
                size_of::<ucontext_t>() + FP_STATE_SIZE,
                align_of::<ucontext_t>(),
            )?;
            let fpu_context_addr = (ucontext_addr as usize) + size_of::<ucontext_t>();
        }
        any(target_arch = "loongarch64", target_arch = "aarch64") => {
            // FIXME: Allocate the FPU context.
            // Example: <https://elixir.bootlin.com/linux/v6.15.7/source/arch/loongarch/kernel/signal.c#L848>
            let ucontext_addr = alloc_aligned_in_user_stack(
                stack_pointer,
                size_of::<ucontext_t>() + fpu_context_bytes.len(),
                align_of::<ucontext_t>(),
            )?;
            // FIXME: Set the FPU flags
            // Example: <https://elixir.bootlin.com/linux/v6.15.7/source/arch/loongarch/kernel/signal.c#L805>
            let fpu_context_addr = (ucontext_addr as usize) + size_of::<ucontext_t>();
        }
        _ => {
            compile_error!("unsupported target");
        }
    }

    // Align the SP to the 16-byte boundary. This is required by the x86-64 ABI before calling any
    // function.
    stack_pointer = ucontext_addr.align_down(16);

    // 3. Write the address of the restorer code.
    let retaddr = if flags.contains(SigActionFlags::SA_RESTORER) {
        restorer_addr
    } else {
        cfg_select! {
            any(target_arch = "riscv64", target_arch = "aarch64") => {
                ctx.user_space().vmar().process_vm().vdso_base()
                    + crate::vdso::__VDSO_RT_SIGRETURN_OFFSET
            }
            _ => {
                // Note that this should already be rejected at the `rt_sigaction` system call.
                return_errno_with_message!(
                    Errno::EINVAL,
                    "this architecture currently requires SA_RESTORER"
                )
            }
        }
    };
    #[cfg(target_arch = "x86_64")]
    let final_sp = stack_pointer
        .checked_sub(size_of::<u64>() as u64)
        .ok_or_else(|| Error::new(Errno::EFAULT))?;
    #[cfg(not(target_arch = "x86_64"))]
    let final_sp = stack_pointer;
    if let Some(bounds) = alternate_bounds
        && (final_sp < bounds.start as u64 || final_sp >= bounds.end as u64)
    {
        return_errno!(Errno::EFAULT);
    }
    user_space.write_val(siginfo_addr as _, &sig_info)?;
    user_space.write_bytes(fpu_context_addr as _, fpu_context_bytes)?;
    user_space.write_val(ucontext_addr as _, &ucontext)?;

    cfg_select! {
        target_arch = "x86_64" => {
            user_space.write_val(final_sp as _, &(retaddr as u64))?;
        }
        any(target_arch = "riscv64", target_arch = "loongarch64") => {
            user_ctx.set_ra(retaddr);
        }
        target_arch = "aarch64" => {
            user_ctx.set_x30(retaddr);
        }
        _ => {
            compile_error!("unsupported target");
        }
    }

    stack_pointer = final_sp;
    ctx.set_sig_mask(old_mask + mask);
    if ctx
        .thread_local
        .sig_stack()
        .borrow()
        .flags()
        .contains(SigStackFlags::SS_AUTODISARM)
    {
        ctx.thread_local.sig_stack().borrow_mut().reset();
    }
    supp.fpu().set(FpuContext::new());

    debug!(
        "Before calling to signal handler: stack_pointer = 0x{:x}",
        stack_pointer
    );

    // 4. Set correct register values.
    #[cfg(target_arch = "x86_64")]
    super::posix_thread::shadow_stack::setup_signal(retaddr, ctx)?;

    user_ctx.set_instruction_pointer(handler_addr as _);
    user_ctx.set_stack_pointer(stack_pointer as usize);
    // Set parameters of the signal handler.
    if flags.contains(SigActionFlags::SA_SIGINFO) {
        user_ctx.set_arguments(sig_num, siginfo_addr as usize, ucontext_addr as usize);
    } else {
        user_ctx.set_arguments(sig_num, 0, 0);
    }
    // Perform CPU architecture-dependent logic.
    cfg_select! {
        target_arch = "x86_64" => {
            // Clear the DF flag. This is to conform to x86-64 calling conventions.
            const X86_RFLAGS_DF: usize = 1 << 10; // Bit 10 is the DF flag.
            user_ctx.set_rflags(user_ctx.rflags() & !X86_RFLAGS_DF);
        }
        _ => {},
    }

    Ok(())
}

/// Uses the alternate signal stack configured via `sigaltstack`.
///
/// If the current stack pointer `sp` is already within the alternate signal stack
/// or if the stack is disabled, this function returns `None`.
/// Otherwise, it returns the starting stack pointer of the alternate signal stack.
fn use_alternate_signal_stack(
    flags: SigActionFlags,
    thread_local: &ThreadLocal,
    sp: usize,
) -> Option<usize> {
    if !flags.contains(SigActionFlags::SA_ONSTACK) {
        return None;
    }

    let sig_stack = thread_local.sig_stack().borrow();

    if sig_stack.active_status(sp) != SigStackStatus::Inactive {
        return None;
    }

    // Overflow checks are done in the `sigaltstack` system call.
    Some(sig_stack.base() + sig_stack.size())
}

/// Allocates `size` bytes on the user's stack.
///
/// The allocation will be aligned to `align`, which must be a power of two.
fn alloc_aligned_in_user_stack(sp: u64, size: usize, align: usize) -> Result<u64> {
    sp.checked_sub(size as u64)
        .map(|address| address.align_down(align as u64))
        .ok_or_else(|| Error::new(Errno::EFAULT))
}
