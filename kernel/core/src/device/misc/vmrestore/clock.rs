// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{Ordering, fence};

use ostd::io::shared_firmware::SharedFirmwareMemory;

use crate::prelude::*;

pub(super) const CLOCK_BYTES: usize = 112;

pub(super) struct Clock(SharedFirmwareMemory);

impl Clock {
    pub(super) fn new(memory: SharedFirmwareMemory) -> Result<Self> {
        let clock = Self(memory);
        let bytes = clock.snapshot()?;
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
        let flags = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        if magic != 0x4b4c4356
            || !(112..=4096).contains(&size)
            || version != 1
            || flags & 0x300 != 0x300
        {
            return_errno_with_message!(Errno::EINVAL, "unsupported VMClock ABI");
        }
        Ok(clock)
    }

    pub(super) fn sequence(&self) -> Result<u32> {
        let sequence = self.0.read_once::<u32>(12)?;
        fence(Ordering::Acquire);
        Ok(sequence)
    }

    pub(super) fn snapshot(&self) -> Result<[u8; CLOCK_BYTES]> {
        for _ in 0..256 {
            let before = self.sequence()?;
            if before & 1 != 0 {
                continue;
            }
            let mut bytes = [0; CLOCK_BYTES];
            self.0.read_bytes(0, &mut bytes)?;
            fence(Ordering::Acquire);
            if before == self.sequence()? {
                return Ok(bytes);
            }
        }
        return_errno_with_message!(Errno::EAGAIN, "VMClock update did not complete");
    }
}
