// SPDX-License-Identifier: MPL-2.0

use core::{num::ParseIntError, str::FromStr};

use ostd::io::shared_firmware::SharedFirmwareMemory;
use spin::Once;

use super::clock::Clock;
use crate::prelude::*;

static CLOCK_ADDR: Once<FirmwareAddress> = Once::new();
static GENID_ADDR: Once<FirmwareAddress> = Once::new();
static IRQ: Once<u32> = Once::new();
aster_cmdline::define_kv_param!("vmrestore.clock_addr", CLOCK_ADDR);
aster_cmdline::define_kv_param!("vmrestore.genid_addr", GENID_ADDR);
aster_cmdline::define_kv_param!("vmrestore.irq", IRQ);

struct FirmwareAddress(usize);

impl FromStr for FirmwareAddress {
    type Err = ParseIntError;

    fn from_str(value: &str) -> core::result::Result<Self, Self::Err> {
        match value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            Some(value) => usize::from_str_radix(value, 16),
            None => value.parse(),
        }
        .map(Self)
    }
}

pub(super) fn acquire() -> Result<(Clock, SharedFirmwareMemory, u32)> {
    let missing = || Error::with_message(Errno::EINVAL, "missing VM restore parameter");
    let clock_addr = CLOCK_ADDR.get().ok_or_else(missing)?.0;
    let genid_addr = GENID_ADDR.get().ok_or_else(missing)?.0;
    let irq = *IRQ.get().ok_or_else(missing)?;
    if !(0x9fc00..=0xdf000).contains(&clock_addr)
        || !clock_addr.is_multiple_of(PAGE_SIZE)
        || !(0x9fc00..=0xdfff0).contains(&genid_addr)
        || !genid_addr.is_multiple_of(8)
        || !(5..=15).contains(&irq)
        || (clock_addr < genid_addr + 16 && genid_addr < clock_addr + PAGE_SIZE)
    {
        return_errno_with_message!(Errno::EINVAL, "invalid VM restore parameters");
    }
    let clock = Clock::new(SharedFirmwareMemory::acquire(
        clock_addr..clock_addr + PAGE_SIZE,
    )?)?;
    let genid = SharedFirmwareMemory::acquire(genid_addr..genid_addr + 16)?;
    Ok((clock, genid, irq))
}
