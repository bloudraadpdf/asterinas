// SPDX-License-Identifier: MPL-2.0

//! Checked inode timestamps and allocation accounting.

use crate::{Error, Geometry, Inode, Result, Timestamp, u16_at, u32_at};

impl Inode {
    pub fn generation(&self) -> u32 {
        u32::from_le_bytes(self.raw[100..104].try_into().unwrap())
    }

    pub fn flags(&self) -> u32 {
        u32::from_le_bytes(self.raw[32..36].try_into().unwrap())
    }

    /// Returns physical allocation in 512-byte sectors, including metadata and attributes.
    pub fn allocated_sectors(&self, geometry: &Geometry) -> u64 {
        let count = u64::from(u32::from_le_bytes(self.raw[28..32].try_into().unwrap()))
            | (u64::from(u16::from_le_bytes(self.raw[116..118].try_into().unwrap())) << 32);
        if self.flags() & 0x0004_0000 != 0 {
            count * u64::from(geometry.block_size() / 512)
        } else {
            count
        }
    }

    pub(super) fn has_data_allocations(&self, geometry: &Geometry) -> Result<bool> {
        let attributes = if self.attribute_block_number()? == 0 {
            0
        } else {
            u64::from(geometry.block_size() / 512)
        };
        Ok(self
            .allocated_sectors(geometry)
            .checked_sub(attributes)
            .ok_or(Error::Corrupt)?
            != 0)
    }

    pub fn access_time(&self) -> Result<Timestamp> {
        self.timestamp(8, 140)
    }
    pub fn change_time(&self) -> Result<Timestamp> {
        self.timestamp(12, 132)
    }
    pub fn modify_time(&self) -> Result<Timestamp> {
        self.timestamp(16, 136)
    }

    pub fn birth_time(&self) -> Result<Option<Timestamp>> {
        if self.raw.len() <= 128 || u16_at(&self.raw, 128)? < 24 {
            return Ok(None);
        }
        self.timestamp(144, 148).map(Some)
    }

    fn timestamp(&self, seconds: usize, extra: usize) -> Result<Timestamp> {
        let low = i64::from(u32_at(&self.raw, seconds)? as i32);
        let extra_size = if self.raw.len() > 128 {
            usize::from(u16_at(&self.raw, 128)?)
        } else {
            0
        };
        let extended = if extra + 4 <= 128 + extra_size {
            u32_at(&self.raw, extra)?
        } else {
            0
        };
        let seconds = low + (i64::from(extended & 3) << 32);
        Timestamp::new(seconds, extended >> 2).map_err(|_| Error::Corrupt)
    }
}
