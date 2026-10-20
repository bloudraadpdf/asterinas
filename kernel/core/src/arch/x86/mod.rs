// SPDX-License-Identifier: MPL-2.0

pub(crate) mod cpu;
mod power;
pub(crate) mod ptrace;
pub(crate) mod signal;

pub(crate) fn init() {
    power::init();
    if let Err(error) = ostd::arch::cpu::context::enable_user_shadow_stacks() {
        ostd::info!("Userspace shadow stacks unavailable: {:?}", error);
    }
}
