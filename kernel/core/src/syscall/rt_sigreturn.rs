// SPDX-License-Identifier: MPL-2.0

use ostd::{arch::cpu::context::UserContext, mm::VmIo, user::UserContextApi};

use super::SyscallReturn;
use crate::{
    prelude::*,
    process::{
        posix_thread::ContextPthreadAdminApi,
        signal::{
            c_types::ucontext_t,
            constants::{SI_KERNEL, SIGSEGV},
            force_fault_signal,
            signals::fault::FaultSignal,
        },
    },
    syscall::sigaltstack::set_new_stack,
};

pub(super) fn sys_rt_sigreturn(ctx: &Context, user_ctx: &mut UserContext) -> Result<SyscallReturn> {
    if let Err(error) = restore_frame(ctx, user_ctx) {
        debug!("invalid signal return frame: {:?}", error);
        force_fault_signal(FaultSignal::new(SIGSEGV, SI_KERNEL, None), ctx);
    }
    Ok(SyscallReturn::NoReturn)
}

fn restore_frame(ctx: &Context, user_ctx: &mut UserContext) -> Result<()> {
    let address = user_ctx.stack_pointer();
    let user_space = ctx.user_space();
    let ucontext = user_space.read_val::<ucontext_t>(address)?;
    #[cfg(target_arch = "x86_64")]
    let fpu_context = crate::process::signal::x86_fpu::decode(
        ucontext.uc_mcontext.fpu_context_addr(),
        &user_space,
    )?;
    #[cfg(any(
        target_arch = "riscv64",
        target_arch = "loongarch64",
        target_arch = "aarch64"
    ))]
    let fpu_context = {
        let fpu_address = address
            .checked_add(size_of::<ucontext_t>())
            .ok_or_else(|| Error::new(Errno::EFAULT))?;
        let mut context = ostd::arch::cpu::context::FpuContext::new();
        user_space.read_bytes(fpu_address, context.as_bytes_mut())?;
        context
    };
    #[cfg(target_arch = "x86_64")]
    crate::process::posix_thread::shadow_stack::restore_signal(ctx)?;
    ucontext.uc_mcontext.copy_user_regs_to(user_ctx);
    let _ = set_new_stack(ucontext.uc_stack, ctx, user_ctx.stack_pointer());
    ctx.thread_local.supp_user_context().fpu().set(fpu_context);
    ctx.set_sig_mask(ucontext.uc_sigmask.into());
    Ok(())
}
