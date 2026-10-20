// SPDX-License-Identifier: MPL-2.0

use core::{
    sync::atomic::{Ordering, fence},
    time::Duration,
};

use spin::Once;
use x86::msr;

use crate::{
    arch::{cpu::cpuid, io::io_mem::read_once},
    cpu::{CpuId, PinCurrentCpu},
    impl_frame_meta_for,
    mm::{self, Frame, FrameAllocOptions, HasPaddr},
    task::disable_preempt,
};

// KVM feature bits and MSR values.
// Reference: <https://elixir.bootlin.com/linux/v7.0/source/arch/x86/include/uapi/asm/kvm_para.h>.
const KVM_FEATURE_CLOCKSOURCE: u32 = 0;
const KVM_FEATURE_CLOCKSOURCE2: u32 = 3;
const MSR_KVM_WALL_CLOCK: u32 = 0x11;
const MSR_KVM_SYSTEM_TIME: u32 = 0x12;
const MSR_KVM_WALL_CLOCK_NEW: u32 = 0x4b56_4d00;
const WALL_CLOCK_OFFSET: usize = 64;
const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
const KVM_MSR_ENABLE_BIT: u64 = 1;

/// Metadata for the KVM pvclock frame.
///
/// The frame is written by the hypervisor and read by the guest clock code,
/// so it is kept typed to prevent OSTD users from reading or writing it as
/// untyped memory via [`VmReader`] and [`VmWriter`].
///
/// [`VmReader`]: crate::mm::VmReader
/// [`VmWriter`]: crate::mm::VmWriter
struct PvclockPageMeta;
impl_frame_meta_for!(PvclockPageMeta);

/// Guest-visible layout of the KVM pvclock page.
///
/// This is Linux's `struct pvclock_vcpu_time_info`. KVM writes this structure
/// directly into the page registered with the selected system-time MSR.
///
/// Reference: <https://elixir.bootlin.com/linux/v7.0/source/arch/x86/include/asm/pvclock-abi.h#L26>.
#[repr(C, align(4))]
struct PvclockVcpuTimeInfo {
    version: u32,
    _pad0: u32,
    tsc_timestamp: u64,
    system_time: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
    _flags: u8,
    _pad: [u8; 2],
}

#[repr(C)]
struct PvclockWallClock {
    version: u32,
    seconds: u32,
    nanoseconds: u32,
}

const _: () = assert!(size_of::<PvclockVcpuTimeInfo>() <= WALL_CLOCK_OFFSET);
const _: () = assert!(WALL_CLOCK_OFFSET + size_of::<PvclockWallClock>() <= mm::PAGE_SIZE);

/// A handle to the KVM pvclock page shared with the hypervisor.
struct PvclockPage(Frame<PvclockPageMeta>);

impl PvclockPage {
    /// Sets up the KVM pvclock page and returns a handle to it.
    fn setup() -> Option<&'static Self> {
        static PVCLOCK_PAGE: Once<PvclockPage> = Once::new();

        let (system_time_msr, wall_clock_msr) = kvm_clock_msrs()?;

        if let Some(page) = PVCLOCK_PAGE.get() {
            return Some(page);
        }

        let frame = FrameAllocOptions::new()
            .alloc_frame_with(PvclockPageMeta)
            .ok()?;
        Some(PVCLOCK_PAGE.call_once(|| {
            // SAFETY: `frame` is a live page that will be retained by
            // `PVCLOCK_PAGE`, and this MSR is supported per
            // `kvm_clock_msrs()` above.
            unsafe {
                msr::wrmsr(system_time_msr, frame.paddr() as u64 | KVM_MSR_ENABLE_BIT);
                // The aligned wall-clock area is disjoint from the vCPU time area
                // and retained in the same typed frame for the hypervisor's lifetime.
                msr::wrmsr(wall_clock_msr, (frame.paddr() + WALL_CLOCK_OFFSET) as u64);
            };
            Self(frame)
        }))
    }

    /// Returns a consistent snapshot read from the KVM pvclock page.
    fn read_time_snapshot(&self) -> Option<PvclockTimeSnapshot> {
        read_consistent(
            || {
                // SAFETY: The retained typed frame contains the aligned vCPU time area.
                unsafe { read_once(core::ptr::addr_of!((*self.as_ptr()).version)) }
            },
            || {
                let ptr = self.as_ptr();
                // SAFETY: The retained typed frame contains each aligned ABI field.
                unsafe {
                    let tsc_timestamp = read_once(core::ptr::addr_of!((*ptr).tsc_timestamp));
                    let system_time = read_once(core::ptr::addr_of!((*ptr).system_time));
                    let tsc_to_system_mul =
                        read_once(core::ptr::addr_of!((*ptr).tsc_to_system_mul));
                    let tsc_shift = read_once(core::ptr::addr_of!((*ptr).tsc_shift));
                    // LFENCE orders the TSC sample between the versioned data reads.
                    // SSE2 and RDTSC are available on this x86-64 target.
                    core::arch::x86_64::_mm_lfence();
                    let current_tsc = core::arch::x86_64::_rdtsc();
                    core::arch::x86_64::_mm_lfence();
                    PvclockTimeSnapshot {
                        tsc_timestamp,
                        system_time,
                        current_tsc,
                        tsc_to_system_mul,
                        tsc_shift,
                    }
                }
            },
        )
        .filter(|time| time.tsc_to_system_mul != 0)
    }

    fn read_wall_offset(&self) -> Option<Duration> {
        let ptr =
            (mm::paddr_to_vaddr(self.0.paddr()) + WALL_CLOCK_OFFSET) as *const PvclockWallClock;
        let (seconds, nanoseconds) = read_consistent(
            || {
                // SAFETY: The aligned wall-clock area is inside the retained typed frame.
                unsafe { read_once(core::ptr::addr_of!((*ptr).version)) }
            },
            || {
                // SAFETY: Both aligned ABI fields are inside the retained typed frame.
                unsafe {
                    (
                        read_once(core::ptr::addr_of!((*ptr).seconds)),
                        read_once(core::ptr::addr_of!((*ptr).nanoseconds)),
                    )
                }
            },
        )?;
        (nanoseconds < 1_000_000_000).then(|| Duration::new(u64::from(seconds), nanoseconds))
    }

    /// Returns a pointer that points to the pvclock page.
    fn as_ptr(&self) -> *const PvclockVcpuTimeInfo {
        mm::paddr_to_vaddr(self.0.paddr()) as *const PvclockVcpuTimeInfo
    }
}

/// A consistent snapshot read from the KVM pvclock page.
struct PvclockTimeSnapshot {
    tsc_timestamp: u64,
    system_time: u64,
    current_tsc: u64,
    tsc_to_system_mul: u32,
    tsc_shift: i8,
}

impl PvclockTimeSnapshot {
    fn system_time_ns(&self) -> Option<u64> {
        if self.tsc_to_system_mul == 0 {
            return None;
        }
        let delta = u128::from(self.current_tsc.checked_sub(self.tsc_timestamp)?);
        let shifted = if self.tsc_shift < 0 {
            delta.checked_shr(i32::from(self.tsc_shift).unsigned_abs())?
        } else {
            delta.checked_mul(1u128.checked_shl(self.tsc_shift as u32)?)?
        };
        let elapsed = shifted.checked_mul(u128::from(self.tsc_to_system_mul))? >> 32;
        self.system_time.checked_add(u64::try_from(elapsed).ok()?)
    }

    /// Returns the TSC frequency in Hz derived from this snapshot.
    fn tsc_freq_hz(&self) -> Option<u64> {
        // The pvclock ABI encodes the TSC frequency using a scale factor
        // (`tsc_to_system_mul`) and a shift (`tsc_shift`) instead of a direct
        // frequency.
        // Reference: <https://elixir.bootlin.com/linux/v7.0/source/arch/x86/kernel/pvclock.c#L27>.

        let base_khz = (1_000_000u64 << 32).checked_div(u64::from(self.tsc_to_system_mul))?;

        let tsc_khz = if self.tsc_shift < 0 {
            base_khz.checked_shl((self.tsc_shift as i32).unsigned_abs())?
        } else {
            base_khz.checked_shr(self.tsc_shift as u32)?
        };

        let freq = tsc_khz.checked_mul(1000)?;
        (freq != 0).then_some(freq)
    }
}

/// Determines the TSC frequency from KVM's paravirtual clock.
pub(super) fn determine_tsc_freq() -> Option<u64> {
    let pvclock_page = PvclockPage::setup()?;
    let time_snapshot = pvclock_page.read_time_snapshot()?;
    time_snapshot.tsc_freq_hz()
}

/// Reads the initial real time from the boot CPU's registered KVM clock.
///
/// Returns `None` on another CPU or when the clock is unavailable or invalid.
pub(in crate::arch) fn read_realtime() -> Option<Duration> {
    let guard = disable_preempt();
    if guard.current_cpu() != CpuId::bsp() {
        return None;
    }
    let page = PvclockPage::setup()?;
    let wall = page.read_wall_offset()?;
    let time = page.read_time_snapshot()?.system_time_ns()?;
    wall.checked_add(Duration::from_nanos(time))
}

fn read_consistent<T>(version: impl Fn() -> u32, data: impl Fn() -> T) -> Option<T> {
    const MAX_RETRIES: usize = 1_000_000;
    for _ in 0..MAX_RETRIES {
        let before = version();
        if before & 1 == 0 {
            // KVM's version protocol requires the fields between these two reads.
            // FIXME: Volatile operations and fences assume the KVM memory model;
            // they do not establish Rust atomic synchronisation with the hypervisor.
            fence(Ordering::Acquire);
            let snapshot = data();
            fence(Ordering::Acquire);
            if before == version() {
                return Some(snapshot);
            }
        }
        core::hint::spin_loop();
    }
    None
}

fn kvm_clock_msrs() -> Option<(u32, u32)> {
    crate::if_tdx_enabled!({ None } else {
        if !cpuid::query_if_running_under_kvm() {
            return None;
        }
        [
            (KVM_FEATURE_CLOCKSOURCE2, MSR_KVM_SYSTEM_TIME_NEW, MSR_KVM_WALL_CLOCK_NEW),
            (KVM_FEATURE_CLOCKSOURCE, MSR_KVM_SYSTEM_TIME, MSR_KVM_WALL_CLOCK),
        ]
        .into_iter()
        .find_map(|(feature, system, wall)| cpuid::query_kvm_feature(feature).then_some((system, wall)))
    })
}

#[cfg(ktest)]
mod test {
    use core::cell::Cell;

    use super::*;
    use crate::prelude::*;

    #[ktest]
    fn pvclock_scales_tsc_delta() {
        let mut time = PvclockTimeSnapshot {
            tsc_timestamp: 100,
            system_time: 42,
            current_tsc: 1100,
            tsc_to_system_mul: 1 << 31,
            tsc_shift: 0,
        };
        assert_eq!(time.system_time_ns(), Some(542));
        time.tsc_shift = 1;
        assert_eq!(time.system_time_ns(), Some(1042));
        time.tsc_shift = -1;
        assert_eq!(time.system_time_ns(), Some(292));
    }

    #[ktest]
    fn pvclock_rejects_invalid_arithmetic() {
        let mut time = PvclockTimeSnapshot {
            tsc_timestamp: 100,
            system_time: 0,
            current_tsc: 99,
            tsc_to_system_mul: 1 << 31,
            tsc_shift: 0,
        };
        assert_eq!(time.system_time_ns(), None);
        time.current_tsc = u64::MAX;
        time.tsc_shift = 127;
        assert_eq!(time.system_time_ns(), None);
        time.tsc_shift = -128;
        assert_eq!(time.system_time_ns(), None);
        time.tsc_shift = 0;
        time.system_time = u64::MAX;
        assert_eq!(time.system_time_ns(), None);
        time.system_time = 0;
        time.tsc_to_system_mul = 0;
        assert_eq!(time.system_time_ns(), None);
    }

    #[ktest]
    fn pvclock_retries_changed_version() {
        let version = Cell::new(0u32);
        let reads = Cell::new(0);
        let snapshot = read_consistent(
            || version.get(),
            || {
                let count = reads.get() + 1;
                reads.set(count);
                if count == 1 {
                    version.set(2);
                }
                count
            },
        );
        assert_eq!(snapshot, Some(2));
        assert_eq!(read_consistent(|| 1, || unreachable!()), None::<()>);
    }
}
