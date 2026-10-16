// SPDX-License-Identifier: MPL-2.0

//! Reseed before any output from a restored random stream.

use core::sync::atomic::{Ordering, fence};

use ostd::io::shared_firmware::SharedFirmwareMemory;

use super::*;

static SOURCE: Once<SharedFirmwareMemory> = Once::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GenerationId([u8; 16]);

pub(crate) fn is_active() -> bool {
    SOURCE.get().is_some()
}

pub(crate) fn current() -> Result<GenerationId> {
    let mut id = [0; 16];
    SOURCE.get().unwrap().read_bytes(0, &mut id)?;
    fence(Ordering::Acquire);
    Ok(GenerationId(id))
}

pub(crate) fn install(source: SharedFirmwareMemory) -> Result<GenerationId> {
    let mut state = RNG.get().unwrap().disable_irq().lock();
    if is_active() {
        return_errno!(Errno::EBUSY);
    }
    SOURCE.call_once(|| source);
    reseed(&mut state)
}

pub(crate) fn refresh() -> Result<GenerationId> {
    reseed(&mut RNG.get().unwrap().disable_irq().lock())
}

pub(crate) fn quiesce() -> Result<()> {
    let mut state = RNG.get().unwrap().disable_irq().lock();
    if state.generation != Some(current()?) {
        reseed(&mut state)?;
    }
    Ok(())
}

pub(super) fn fill(state: &mut RandomState, dst: &mut [u8]) -> Result<()> {
    for _ in 0..256 {
        let generation = current()?;
        if state.generation != Some(generation) {
            reseed(state)?;
        }
        state.rng.fill_bytes(dst);
        if state.generation == Some(current()?) {
            return Ok(());
        }
    }
    return_errno_with_message!(Errno::EAGAIN, "generation changed during random output");
}

fn reseed(state: &mut RandomState) -> Result<GenerationId> {
    for _ in 0..256 {
        let before = current()?;
        let seed = read_seed_from_hardware().ok_or_else(|| {
            Error::with_message(Errno::EIO, "fresh hardware entropy is unavailable")
        })?;
        if before == current()? {
            state.rng = StdRng::from_seed(seed);
            state.generation = Some(before);
            return Ok(before);
        }
    }
    return_errno_with_message!(Errno::EAGAIN, "generation changed during entropy refresh");
}
