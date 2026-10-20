// SPDX-License-Identifier: MPL-2.0

//! Per-thread userspace CET registers, kept separate from user-restorable FPU state.

use core::sync::atomic::{AtomicU8, Ordering};

use spin::Once;
use x86::msr::{rdmsr, wrmsr};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

use crate::{
    cpu::{CpuId, CpuSet, PinCurrentCpu, all_cpus},
    cpu_local, cpu_local_cell,
    irq::DisabledLocalIrqGuard,
    mm::{MAX_USERSPACE_VADDR, PAGE_SIZE, UFrame, io::util::HasVmReaderWriter},
};

const IA32_U_CET: u32 = 0x6a0;
const IA32_S_CET: u32 = 0x6a2;
const IA32_PL3_SSP: u32 = 0x6a7;
const SHADOW_STACK_ENABLE: u64 = 1;
const WRSS_ENABLE: u64 = 1 << 1;

cpu_local_cell! {
    static USER_CET_LOADED: u8 = 0;
}

const CPU_UNSUPPORTED: u8 = 0;
const CPU_SUPPORTED: u8 = 1;
const CPU_SUPERVISOR_CET_ACTIVE: u8 = 2;
const CPU_WRITE_PROTECTION_DISABLED: u8 = 3;
const CPU_ENABLED: u8 = 4;

cpu_local! {
    static USER_CET_ADMISSION: AtomicU8 = AtomicU8::new(CPU_UNSUPPORTED);
}

static USER_SHADOW_STACKS: Once<Result<(), UserShadowStackAdmissionError>> = Once::new();

/// A processor that prevents system-wide userspace shadow-stack admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserShadowStackAdmissionError {
    /// The processor does not support shadow stacks.
    UnsupportedCpu(CpuId),
    /// The processor already has supervisor CET enabled.
    SupervisorCetActive(CpuId),
    /// The processor has CR0.WP cleared, which forbids enabling CR4.CET.
    WriteProtectionDisabledCpu(CpuId),
}

/// Enables user-only CET on every runnable processor before publishing availability.
///
/// All processors must have completed OSTD initialisation, including the IPI module.
/// Local IRQs must be enabled. This is permanent admission: CPU hotplug and disabling
/// CR4.CET after this call are not supported. No thread is enabled by this operation.
/// The kernel must implement shadow-stack mappings, faults and thread/signal lifecycles
/// before it calls this function or exposes the feature to userspace.
///
/// # Panics
///
/// Panics if IPI initialisation is incomplete or local IRQs are disabled.
pub fn enable_user_shadow_stacks() -> Result<(), UserShadowStackAdmissionError> {
    *USER_SHADOW_STACKS.call_once(|| {
        assert!(crate::arch::irq::is_local_enabled());
        let cpus = CpuSet::new_full();
        crate::smp::inter_processor_call(&cpus, probe_user_cet).wait();
        for cpu in all_cpus() {
            match USER_CET_ADMISSION.get_on_cpu(cpu).load(Ordering::Acquire) {
                CPU_SUPPORTED => {}
                CPU_SUPERVISOR_CET_ACTIVE => {
                    return Err(UserShadowStackAdmissionError::SupervisorCetActive(cpu));
                }
                CPU_WRITE_PROTECTION_DISABLED => {
                    return Err(UserShadowStackAdmissionError::WriteProtectionDisabledCpu(
                        cpu,
                    ));
                }
                _ => return Err(UserShadowStackAdmissionError::UnsupportedCpu(cpu)),
            }
        }
        crate::smp::inter_processor_call(&cpus, enable_user_cet_on_cpu).wait();
        for cpu in all_cpus() {
            assert_eq!(
                USER_CET_ADMISSION.get_on_cpu(cpu).load(Ordering::Acquire),
                CPU_ENABLED
            );
        }
        Ok(())
    })
}

fn cpu_supports_shadow_stacks() -> bool {
    super::super::cpuid::cpuid(7, 0).is_some_and(|features| features.ecx & (1 << 7) != 0)
}

fn probe_user_cet() {
    let guard = crate::irq::disable_local();
    let admission = USER_CET_ADMISSION.get_on_cpu(guard.current_cpu());
    if !cpu_supports_shadow_stacks() {
        admission.store(CPU_UNSUPPORTED, Ordering::Release);
        return;
    }
    if !Cr0::read().contains(Cr0Flags::WRITE_PROTECT) {
        admission.store(CPU_WRITE_PROTECTION_DISABLED, Ordering::Release);
        return;
    }
    // SAFETY: SHSTK CPUID guarantees the CET MSR. The read leaves supervisor state unchanged.
    let supervisor_control = unsafe { rdmsr(IA32_S_CET) };
    admission.store(
        if supervisor_control == 0 {
            CPU_SUPPORTED
        } else {
            CPU_SUPERVISOR_CET_ACTIVE
        },
        Ordering::Release,
    );
}

fn enable_user_cet_on_cpu() {
    let guard = crate::irq::disable_local();
    let admission = USER_CET_ADMISSION.get_on_cpu(guard.current_cpu());
    assert_eq!(admission.load(Ordering::Acquire), CPU_SUPPORTED);
    assert!(cpu_supports_shadow_stacks());
    assert!(Cr0::read().contains(Cr0Flags::WRITE_PROTECT));
    // SAFETY: Every runnable CPU passed the SHSTK and inactive-supervisor checks. Local IRQs
    // are disabled and CR0.WP is set. User CET is cleared before CR4.CET is set; supervisor
    // CET stays disabled.
    unsafe {
        wrmsr(IA32_U_CET, 0);
        wrmsr(IA32_PL3_SSP, 0);
        Cr4::write(Cr4::read() | Cr4Flags::CONTROL_FLOW_ENFORCEMENT);
    }
    admission.store(CPU_ENABLED, Ordering::Release);
}

/// An invalid or unavailable userspace shadow-stack context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowStackContextError {
    /// Userspace CET is not enabled on the current CPU.
    Unsupported,
    /// The stack pointer is null, unaligned, or outside userspace.
    StackPointer,
    /// The operation requires an enabled shadow stack.
    Disabled,
}

/// A validated userspace CET control register and shadow-stack pointer.
///
/// A default context disables both shadow stacks and WRSS. Constructing an
/// enabled context requires CPU support and OS enablement through CR4.CET.
/// The kernel remains responsible for the stack mapping and its ownership.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UserShadowStackContext {
    control: u64,
    stack_pointer: usize,
}

impl UserShadowStackContext {
    /// Checks whether the current CPU supports OS-enabled userspace CET.
    pub fn is_available() -> bool {
        USER_SHADOW_STACKS.get().is_some_and(Result::is_ok)
            && cpu_supports_shadow_stacks()
            && Cr4::read().contains(Cr4Flags::CONTROL_FLOW_ENFORCEMENT)
    }

    /// Creates an enabled shadow-stack context with WRSS disabled.
    pub fn new(stack_pointer: usize) -> Result<Self, ShadowStackContextError> {
        validate_stack_pointer(stack_pointer)?;
        if !Self::is_available() {
            return Err(ShadowStackContextError::Unsupported);
        }
        Ok(Self {
            control: SHADOW_STACK_ENABLE,
            stack_pointer,
        })
    }

    /// Returns whether the context enables a userspace shadow stack.
    pub fn is_enabled(&self) -> bool {
        self.control & SHADOW_STACK_ENABLE != 0
    }

    /// Returns whether the context permits userspace WRSS instructions.
    pub fn wrss_enabled(&self) -> bool {
        self.control & WRSS_ENABLE != 0
    }

    /// Returns the shadow-stack pointer if the context is enabled.
    pub fn stack_pointer(&self) -> Option<usize> {
        self.is_enabled().then_some(self.stack_pointer)
    }

    /// Updates the pointer of an enabled shadow stack.
    pub fn set_stack_pointer(&mut self, address: usize) -> Result<(), ShadowStackContextError> {
        if !self.is_enabled() {
            return Err(ShadowStackContextError::Disabled);
        }
        validate_stack_pointer(address)?;
        self.stack_pointer = address;
        Ok(())
    }

    /// Restores an enabled stack's SSP after ptrace or signal-token validation.
    ///
    /// Zero and unmapped userspace are valid register values and fault normally
    /// when accessed. Only canonical, aligned values can reach the hardware.
    pub fn set_restored_stack_pointer(
        &mut self,
        address: usize,
    ) -> Result<(), ShadowStackContextError> {
        if !self.is_enabled() {
            return Err(ShadowStackContextError::Disabled);
        }
        if address >= MAX_USERSPACE_VADDR || !address.is_multiple_of(8) {
            return Err(ShadowStackContextError::StackPointer);
        }
        self.stack_pointer = address;
        Ok(())
    }

    /// Enables WRSS for an enabled shadow stack.
    pub fn enable_wrss(&mut self) -> Result<(), ShadowStackContextError> {
        if !self.is_enabled() {
            return Err(ShadowStackContextError::Disabled);
        }
        self.control |= WRSS_ENABLE;
        Ok(())
    }

    /// Disables userspace WRSS instructions.
    pub fn disable_wrss(&mut self) {
        self.control &= !WRSS_ENABLE;
    }

    /// Saves the current userspace CET registers without changing hardware state.
    ///
    /// # Panics
    ///
    /// Panics if OS-enabled CET becomes unavailable after an enabled context was loaded.
    pub fn save(&mut self, _guard: &DisabledLocalIrqGuard) {
        if USER_CET_LOADED.load() == 0 {
            *self = Self::default();
            return;
        }
        assert!(Self::is_available(), "loaded userspace CET is unavailable");
        // SAFETY: The current CPU supports CET and local IRQs are disabled. These
        // reads preserve its user-only registers and do not affect kernel execution.
        unsafe {
            self.control = rdmsr(IA32_U_CET);
            self.stack_pointer = rdmsr(IA32_PL3_SSP) as usize;
        }
        debug_assert_eq!(self.control & !(SHADOW_STACK_ENABLE | WRSS_ENABLE), 0);
    }

    /// Restores userspace CET, clearing a previously loaded thread's state if disabled.
    ///
    /// Never-loaded disabled contexts perform no MSR access.
    ///
    /// # Panics
    ///
    /// Panics if an enabled context is restored on a CPU without OS-enabled CET.
    /// The scheduler must admit enabled threads only to CPUs with this support.
    pub fn load(&self, _guard: &DisabledLocalIrqGuard) {
        if !self.is_enabled() && USER_CET_LOADED.load() == 0 {
            return;
        }
        assert!(
            Self::is_available(),
            "userspace CET is unavailable on this CPU"
        );
        // SAFETY: CPU support and CR4.CET are checked above. Private fields hold
        // validated control bits and a canonical SSP. Local IRQs prevent migration
        // between the register writes and the CPU-local ownership update. These
        // MSRs affect CPL3 only; supervisor CET is unchanged.
        unsafe {
            wrmsr(IA32_U_CET, 0);
            wrmsr(IA32_PL3_SSP, self.stack_pointer as u64);
            if self.is_enabled() {
                wrmsr(IA32_U_CET, self.control);
            }
        }
        USER_CET_LOADED.store(u8::from(self.is_enabled()));
    }
}

fn validate_stack_pointer(address: usize) -> Result<(), ShadowStackContextError> {
    if address == 0 || address >= MAX_USERSPACE_VADDR || !address.is_multiple_of(8) {
        return Err(ShadowStackContextError::StackPointer);
    }
    Ok(())
}

#[cfg(ktest)]
mod tests {
    use super::*;
    use crate::prelude::*;

    #[ktest]
    fn disabled_context_cannot_enable_wrss_or_install_a_pointer() {
        let mut context = UserShadowStackContext::default();
        assert!(!context.is_enabled());
        assert!(!context.wrss_enabled());
        assert_eq!(context.stack_pointer(), None);
        assert_eq!(
            context.enable_wrss(),
            Err(ShadowStackContextError::Disabled)
        );
        assert_eq!(
            context.set_stack_pointer(0x1000),
            Err(ShadowStackContextError::Disabled)
        );
    }

    #[ktest]
    fn new_context_rejects_unaligned_and_non_userspace_pointers() {
        for address in [0, 1, 0x1001, MAX_USERSPACE_VADDR, usize::MAX] {
            assert_eq!(
                UserShadowStackContext::new(address),
                Err(ShadowStackContextError::StackPointer)
            );
        }
    }

    #[ktest]
    fn restored_stack_pointer_accepts_only_aligned_userspace_values() {
        let mut context = UserShadowStackContext {
            control: SHADOW_STACK_ENABLE,
            stack_pointer: 0x1000,
        };
        assert_eq!(context.set_restored_stack_pointer(0), Ok(()));
        assert_eq!(context.stack_pointer(), Some(0));
        assert_eq!(
            context.set_restored_stack_pointer(1),
            Err(ShadowStackContextError::StackPointer)
        );
        assert_eq!(
            context.set_restored_stack_pointer(MAX_USERSPACE_VADDR),
            Err(ShadowStackContextError::StackPointer)
        );
        assert_eq!(
            context.set_stack_pointer(0),
            Err(ShadowStackContextError::StackPointer)
        );
    }

    #[ktest]
    fn disabled_context_does_not_require_cet_enablement() {
        let guard = crate::irq::disable_local();
        let mut context = UserShadowStackContext::default();
        context.load(&guard);
        context.save(&guard);
        assert_eq!(context, UserShadowStackContext::default());
    }
}

/// Writes one aligned word through the owned user frame's ordinary kernel alias.
///
/// The caller validates the shadow-stack VMA and resolves its COW state while
/// holding the page-table cursor. This bypasses no user mapping permissions and
/// never enables WRSS. Keeping the frame owned makes the physical alias valid.
pub fn write_shadow_stack_word(
    frame: &UFrame,
    offset: usize,
    value: u64,
) -> Result<(), ShadowStackContextError> {
    if !offset.is_multiple_of(8) || offset > PAGE_SIZE - size_of::<u64>() {
        return Err(ShadowStackContextError::StackPointer);
    }
    frame
        .writer()
        .skip(offset)
        .write_val(&value)
        .map_err(|_| ShadowStackContextError::StackPointer)
}

#[cfg(ktest)]
mod alias_tests {
    use super::*;
    use crate::{mm::FrameAllocOptions, prelude::*};

    #[ktest]
    fn shadow_stack_alias_write_checks_alignment_and_frame_bounds() {
        let frame: UFrame = FrameAllocOptions::new().alloc_frame().unwrap().into();
        assert_eq!(
            write_shadow_stack_word(&frame, PAGE_SIZE - 8, 0x12345678),
            Ok(())
        );
        assert_eq!(
            frame
                .reader()
                .skip(PAGE_SIZE - 8)
                .read_val::<u64>()
                .unwrap(),
            0x12345678
        );
        assert_eq!(
            write_shadow_stack_word(&frame, 1, 1),
            Err(ShadowStackContextError::StackPointer)
        );
        assert_eq!(
            write_shadow_stack_word(&frame, PAGE_SIZE, 1),
            Err(ShadowStackContextError::StackPointer)
        );
    }
}
