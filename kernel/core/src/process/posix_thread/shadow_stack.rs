// SPDX-License-Identifier: MPL-2.0

use core::{
    num::NonZeroU64,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use align_ext::AlignExt;
use ostd::{arch::cpu::context::UserShadowStackContext, mm::MAX_USERSPACE_VADDR};

use crate::{
    prelude::*,
    process::{clone::CloneFlags, rlimit::ResourceType},
    vm::vmar::{Vmar, VmarHandle},
};

const SHSTK: u64 = 1;
const WRSS: u64 = 2;
const SIGNAL_TOKEN: u64 = 1 << 63;

struct OwnedShadowStack {
    vmar: Arc<Vmar>,
    range: Range<Vaddr>,
}

impl Drop for OwnedShadowStack {
    fn drop(&mut self) {
        self.vmar.remove_mapping(self.range.clone());
    }
}

#[derive(Default)]
pub(crate) struct ShadowStackControl {
    features: AtomicU64,
    locked: AtomicU64,
}

impl ShadowStackControl {
    pub(crate) fn features(&self) -> u64 {
        self.features.load(Ordering::Acquire)
    }
    pub(crate) fn locked(&self) -> u64 {
        self.locked.load(Ordering::Acquire)
    }
    pub(crate) fn lock(&self, mask: u64) {
        self.locked.fetch_or(mask, Ordering::AcqRel);
    }
    pub(crate) fn unlock(&self, mask: u64) {
        self.locked.fetch_and(!mask, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(crate) struct ShadowStackState {
    owned: Option<OwnedShadowStack>,
    control: Arc<ShadowStackControl>,
}

impl ShadowStackState {
    pub(crate) fn features(ctx: &Context) -> u64 {
        ctx.posix_thread.shadow_stack_control().features()
    }

    pub(crate) fn lock(&mut self, mask: u64) {
        self.control.lock(mask);
    }

    pub(super) fn shared(&self) -> Arc<ShadowStackControl> {
        self.control.clone()
    }

    pub(crate) fn control(&mut self, enable: bool, feature: u64, ctx: &Context) -> Result<()> {
        if feature & self.control.locked() != 0 {
            return_errno!(Errno::EPERM);
        }
        if !matches!(feature, SHSTK | WRSS) {
            return_errno!(Errno::EINVAL);
        }
        if !UserShadowStackContext::is_available() {
            return_errno!(Errno::EOPNOTSUPP);
        }
        let register = ctx.thread_local.supp_user_context().shadow_stack();
        let mut registers = register.get();
        match (feature, enable) {
            (SHSTK, true) if !registers.is_enabled() => {
                let size = stack_size(ctx, None)?;
                let vmar = ctx.thread_local.vmar().borrow();
                let vmar = vmar.as_ref().unwrap();
                let address = vmar.map_shadow_stack(0, size)?;
                let owned = OwnedShadowStack {
                    vmar: vmar.clone_arc(),
                    range: address..address + size,
                };
                registers = UserShadowStackContext::new(address + size)
                    .map_err(|_| Error::new(Errno::EOPNOTSUPP))?;
                self.owned = Some(owned);
            }
            (SHSTK, false) => {
                register.set(UserShadowStackContext::default());
                self.control.features.store(0, Ordering::Release);
                self.owned = None;
                return Ok(());
            }
            (WRSS, _) if !registers.is_enabled() => return_errno!(Errno::EPERM),
            (WRSS, true) => registers
                .enable_wrss()
                .map_err(|_| Error::new(Errno::EPERM))?,
            (WRSS, false) => registers.disable_wrss(),
            _ => {}
        }
        register.set(registers);
        self.control.features.store(
            (u64::from(registers.is_enabled()) * SHSTK)
                | (u64::from(registers.wrss_enabled()) * WRSS),
            Ordering::Release,
        );
        Ok(())
    }

    pub(crate) fn clone_for(
        &self,
        ctx: &Context,
        child_vmar: &VmarHandle,
        flags: CloneFlags,
        requested_size: Option<NonZeroU64>,
    ) -> Result<(Self, UserShadowStackContext)> {
        let mut registers = ctx.thread_local.supp_user_context().shadow_stack().get();
        let mut child = Self {
            owned: None,
            control: Arc::new(ShadowStackControl {
                features: AtomicU64::new(self.control.features()),
                locked: AtomicU64::new(self.control.locked()),
            }),
        };
        if !registers.is_enabled() || flags.contains(CloneFlags::CLONE_VFORK) {
            return Ok((child, registers));
        }
        if flags.contains(CloneFlags::CLONE_VM) {
            let size = stack_size(ctx, requested_size)?;
            let address = child_vmar.map_shadow_stack(0, size)?;
            child.owned = Some(OwnedShadowStack {
                vmar: child_vmar.clone_arc(),
                range: address..address + size,
            });
            registers
                .set_stack_pointer(address + size)
                .map_err(|_| Error::new(Errno::EINVAL))?;
        } else if let Some(owned) = &self.owned {
            child.owned = Some(OwnedShadowStack {
                vmar: child_vmar.clone_arc(),
                range: owned.range.clone(),
            });
        }
        Ok((child, registers))
    }
}

fn stack_size(ctx: &Context, requested: Option<NonZeroU64>) -> Result<usize> {
    let size = requested.map_or_else(
        || {
            ctx.process
                .resource_limits()
                .get_rlimit(ResourceType::RLIMIT_STACK)
                .get_raw_rlimit()
                .cur
                .min(1 << 32)
        },
        NonZeroU64::get,
    );
    let size = usize::try_from(size).map_err(|_| Error::new(Errno::ENOMEM))?;
    let aligned = size
        .checked_add(PAGE_SIZE - 1)
        .ok_or_else(|| Error::new(Errno::ENOMEM))?
        .align_down(PAGE_SIZE);
    if aligned == 0 {
        return_errno!(Errno::EINVAL);
    }
    Ok(aligned)
}

pub(crate) fn reset(ctx: &Context) {
    ctx.thread_local
        .supp_user_context()
        .shadow_stack()
        .set(UserShadowStackContext::default());
    let mut state = ctx.thread_local.shadow_stack().borrow_mut();
    state.control.features.store(0, Ordering::Release);
    state.control.locked.store(0, Ordering::Release);
    state.owned = None;
}

fn signal_context(ctx: &Context) -> Result<Option<(UserShadowStackContext, Vaddr)>> {
    let registers = ctx.thread_local.supp_user_context().shadow_stack().get();
    let Some(pointer) = registers.stack_pointer() else {
        return Ok(None);
    };
    if pointer == 0 || !pointer.is_multiple_of(8) {
        return_errno!(Errno::EINVAL);
    }
    Ok(Some((registers, pointer)))
}

pub(crate) fn setup_signal(restorer: Vaddr, ctx: &Context) -> Result<()> {
    let Some((mut registers, pointer)) = signal_context(ctx)? else {
        return Ok(());
    };
    if restorer == 0 {
        return_errno!(Errno::EINVAL);
    }
    let next = pointer
        .checked_sub(16)
        .ok_or_else(|| Error::new(Errno::EFAULT))?;
    let vmar = ctx.user_space();
    vmar.vmar()
        .write_shadow_stack_word(pointer - 8, pointer as u64 | SIGNAL_TOKEN)?;
    vmar.vmar().write_shadow_stack_word(next, restorer as u64)?;
    registers
        .set_stack_pointer(next)
        .map_err(|_| Error::new(Errno::EFAULT))?;
    ctx.thread_local
        .supp_user_context()
        .shadow_stack()
        .set(registers);
    Ok(())
}

pub(crate) fn restore_signal(ctx: &Context) -> Result<()> {
    let Some((mut registers, pointer)) = signal_context(ctx)? else {
        return Ok(());
    };
    let token = ctx
        .user_space()
        .vmar()
        .read_shadow_stack_signal_token(pointer)?;
    let target = (token & !SIGNAL_TOKEN) as usize;
    if token & SIGNAL_TOKEN == 0 || target >= MAX_USERSPACE_VADDR || !target.is_multiple_of(8) {
        return_errno!(Errno::EINVAL);
    }
    registers
        .set_restored_stack_pointer(target)
        .map_err(|_| Error::new(Errno::EINVAL))?;
    ctx.thread_local
        .supp_user_context()
        .shadow_stack()
        .set(registers);
    Ok(())
}
