// SPDX-License-Identifier: MPL-2.0

//! Superblock geometry and checksum validation.

use io_util::checksum;

use super::{Error, Result, bytes_at, u16_at, u32_at};

const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_SUPPORTED: u32 =
    0x0002 | INCOMPAT_RECOVER | INCOMPAT_EXTENTS | INCOMPAT_64BIT | 0x0200 | INCOMPAT_CSUM_SEED;
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;
const RO_COMPAT_SUPPORTED: u32 =
    0x0001 | 0x0002 | 0x0008 | 0x0020 | 0x0040 | RO_COMPAT_METADATA_CSUM | 0x10000;

/// Geometry checked against the block device and supported disk features.
#[derive(Clone, Debug)]
pub struct Geometry {
    pub(super) raw: [u8; 1024],
    blocks: u64,
    inodes: u32,
    block_size: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    groups: u32,
    inode_size: u16,
    descriptor_size: u16,
    first_data_block: u32,
    checksum_seed: u32,
    incompatible: u32,
    read_only_compatible: u32,
}

impl Geometry {
    /// Parses a complete on-disk superblock and validates its device bounds.
    pub fn parse(raw: [u8; 1024], device_size: u64) -> Result<Self> {
        if u16_at(&raw, 0x38)? != 0xef53 || u32_at(&raw, 0x4c)? != 1 || u32_at(&raw, 0x48)? != 0 {
            return Err(Error::Corrupt);
        }
        let incompatible = u32_at(&raw, 0x60)?;
        let read_only_compatible = u32_at(&raw, 0x64)?;
        if incompatible & !INCOMPAT_SUPPORTED != 0
            || incompatible & INCOMPAT_EXTENTS == 0
            || read_only_compatible & !RO_COMPAT_SUPPORTED != 0
        {
            return Err(Error::Unsupported);
        }
        if read_only_compatible & RO_COMPAT_METADATA_CSUM != 0 {
            if raw[0x175] != 1 {
                return Err(Error::Unsupported);
            }
            if checksum::crc32c(!0, &raw[..0x3fc]) != u32_at(&raw, 0x3fc)? {
                return Err(Error::Corrupt);
            }
        }
        let log_block_size = u32_at(&raw, 0x18)?;
        if log_block_size > 2 || u32_at(&raw, 0x1c)? != log_block_size {
            return Err(Error::Unsupported);
        }
        let block_size = 1024 << log_block_size;
        let first_data_block = u32_at(&raw, 0x14)?;
        if first_data_block != u32::from(block_size == 1024) {
            return Err(Error::Corrupt);
        }
        let blocks = u64::from(u32_at(&raw, 0x04)?)
            | if incompatible & INCOMPAT_64BIT != 0 {
                u64::from(u32_at(&raw, 0x150)?) << 32
            } else {
                0
            };
        let inodes = u32_at(&raw, 0)?;
        let blocks_per_group = u32_at(&raw, 0x20)?;
        let inodes_per_group = u32_at(&raw, 0x28)?;
        if blocks <= u64::from(first_data_block)
            || blocks
                .checked_mul(u64::from(block_size))
                .is_none_or(|size| size > device_size)
            || inodes < 11
            || blocks_per_group == 0
            || blocks_per_group > block_size * 8
            || !blocks_per_group.is_multiple_of(8)
            || inodes_per_group == 0
            || inodes_per_group > block_size * 8
            || !inodes_per_group.is_multiple_of(8)
            || u32_at(&raw, 0x24)? != blocks_per_group
        {
            return Err(Error::Corrupt);
        }
        let groups = u32::try_from(
            (blocks - u64::from(first_data_block)).div_ceil(u64::from(blocks_per_group)),
        )
        .map_err(|_| Error::Corrupt)?;
        if u64::from(inodes) > u64::from(groups) * u64::from(inodes_per_group)
            || u64::from(inodes) <= u64::from(groups - 1) * u64::from(inodes_per_group)
        {
            return Err(Error::Corrupt);
        }
        let inode_size = u16_at(&raw, 0x58)?;
        if inode_size < 128 || u32::from(inode_size) > block_size || !inode_size.is_power_of_two() {
            return Err(Error::Corrupt);
        }
        let descriptor_size = if incompatible & INCOMPAT_64BIT != 0 {
            u16_at(&raw, 0xfe)?
        } else {
            32
        };
        if descriptor_size < 32
            || (incompatible & INCOMPAT_64BIT != 0 && descriptor_size < 64)
            || u32::from(descriptor_size) > block_size
            || descriptor_size > 1024
            || !descriptor_size.is_power_of_two()
        {
            return Err(Error::Corrupt);
        }
        let descriptors_end = (u64::from(first_data_block) + 1) * u64::from(block_size)
            + u64::from(groups) * u64::from(descriptor_size);
        if descriptors_end > blocks * u64::from(block_size) {
            return Err(Error::Corrupt);
        }
        let checksum_seed = if incompatible & INCOMPAT_CSUM_SEED != 0 {
            if read_only_compatible & RO_COMPAT_METADATA_CSUM == 0 {
                return Err(Error::Corrupt);
            }
            u32_at(&raw, 0x270)?
        } else {
            checksum::crc32c(!0, &raw[0x68..0x78])
        };
        Ok(Self {
            raw,
            blocks,
            inodes,
            block_size,
            blocks_per_group,
            inodes_per_group,
            groups,
            inode_size,
            descriptor_size,
            first_data_block,
            checksum_seed,
            incompatible,
            read_only_compatible,
        })
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }
    pub fn inodes(&self) -> u32 {
        self.inodes
    }
    pub fn block_size(&self) -> u32 {
        self.block_size
    }
    pub fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }
    pub fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }
    pub fn groups(&self) -> u32 {
        self.groups
    }
    pub fn free_blocks(&self) -> u64 {
        u64::from(u32::from_le_bytes(self.raw[0x0c..0x10].try_into().unwrap()))
            | if self.incompatible & INCOMPAT_64BIT != 0 {
                u64::from(u32::from_le_bytes(
                    self.raw[0x158..0x15c].try_into().unwrap(),
                )) << 32
            } else {
                0
            }
    }
    /// Returns free blocks after the superblock's reserved-block allowance.
    pub fn available_blocks(&self) -> u64 {
        let reserved = u64::from(u32::from_le_bytes(self.raw[0x08..0x0c].try_into().unwrap()))
            | if self.incompatible & INCOMPAT_64BIT != 0 {
                u64::from(u32::from_le_bytes(
                    self.raw[0x154..0x158].try_into().unwrap(),
                )) << 32
            } else {
                0
            };
        self.free_blocks().saturating_sub(reserved)
    }
    pub fn free_inodes(&self) -> u32 {
        u32::from_le_bytes(self.raw[0x10..0x14].try_into().unwrap())
    }

    pub(super) fn write_free_counts(
        &self,
        raw: &mut [u8; 1024],
        blocks: u64,
        inodes: u32,
    ) -> Result<()> {
        if blocks > self.blocks || inodes > self.inodes {
            return Err(Error::Corrupt);
        }
        raw[0x0c..0x10].copy_from_slice(&(blocks as u32).to_le_bytes());
        if self.incompatible & INCOMPAT_64BIT != 0 {
            raw[0x158..0x15c].copy_from_slice(&((blocks >> 32) as u32).to_le_bytes());
        }
        raw[0x10..0x14].copy_from_slice(&inodes.to_le_bytes());
        Ok(())
    }

    pub(super) fn group_start(&self, group: u32) -> u64 {
        u64::from(self.first_data_block) + u64::from(group) * u64::from(self.blocks_per_group)
    }

    pub(super) fn has_backup_superblock(&self, group: u32) -> bool {
        if group == 0 {
            return true;
        }
        let compatible = u32::from_le_bytes(self.raw[0x5c..0x60].try_into().unwrap());
        if compatible & 0x200 != 0 {
            return [0x24c, 0x250].iter().any(|offset| {
                u32::from_le_bytes(self.raw[*offset..*offset + 4].try_into().unwrap()) == group
            });
        }
        if self.read_only_compatible & 1 == 0 || group == 1 {
            return true;
        }
        [3, 5, 7].iter().any(|base| {
            let mut value = group;
            while value.is_multiple_of(*base) {
                value /= base;
            }
            value == 1
        })
    }
    pub fn inode_size(&self) -> u16 {
        self.inode_size
    }
    pub fn descriptor_size(&self) -> u16 {
        self.descriptor_size
    }
    pub fn checksum_seed(&self) -> u32 {
        self.checksum_seed
    }
    pub fn uuid(&self) -> [u8; 16] {
        bytes_at(&self.raw, 0x68).unwrap()
    }
    pub fn has_metadata_checksums(&self) -> bool {
        self.read_only_compatible & RO_COMPAT_METADATA_CSUM != 0
    }

    pub fn has_directory_types(&self) -> bool {
        self.incompatible & 2 != 0
    }
    pub fn has_large_directory_link_counts(&self) -> bool {
        self.read_only_compatible & 0x20 != 0
    }
    pub fn needs_recovery(&self) -> bool {
        self.incompatible & INCOMPAT_RECOVER != 0
    }
    pub fn is_clean(&self) -> bool {
        u16::from_le_bytes(self.raw[0x3a..0x3c].try_into().unwrap()) & 1 != 0
    }
    pub fn has_errors(&self) -> bool {
        u16::from_le_bytes(self.raw[0x3a..0x3c].try_into().unwrap()) & 2 != 0
    }
    pub fn journal_inode(&self) -> u32 {
        if u32::from_le_bytes(self.raw[0x5c..0x60].try_into().unwrap()) & 4 != 0 {
            u32::from_le_bytes(self.raw[0xe0..0xe4].try_into().unwrap())
        } else {
            0
        }
    }
    pub fn has_orphan_file(&self) -> bool {
        u32::from_le_bytes(self.raw[0x5c..0x60].try_into().unwrap()) & 0x1000 != 0
    }
    pub fn orphan_file_inode(&self) -> u32 {
        u32::from_le_bytes(self.raw[0x280..0x284].try_into().unwrap())
    }
    pub fn orphan_cleanup_required(&self) -> bool {
        self.read_only_compatible & 0x10000 != 0
    }
    pub fn last_orphan(&self) -> u32 {
        u32::from_le_bytes(self.raw[0xe8..0xec].try_into().unwrap())
    }

    pub(super) fn recovery_superblock(&self) -> [u8; 1024] {
        let mut raw = self.raw;
        raw[0x60..0x64].copy_from_slice(&(self.incompatible | INCOMPAT_RECOVER).to_le_bytes());
        self.set_super_checksum(&mut raw);
        raw
    }

    pub(super) fn mounted_superblock(
        &self,
        free_blocks: u64,
        free_inodes: u32,
    ) -> Result<[u8; 1024]> {
        let mut raw = self.raw;
        if self.journal_inode() != 0 {
            raw[0x60..0x64].copy_from_slice(&(self.incompatible | INCOMPAT_RECOVER).to_le_bytes());
        }
        if self.has_orphan_file() {
            raw[0x64..0x68].copy_from_slice(&(self.read_only_compatible | 0x10000).to_le_bytes());
        }
        let state = u16::from_le_bytes(raw[0x3a..0x3c].try_into().unwrap());
        raw[0x3a..0x3c].copy_from_slice(&(state & !1).to_le_bytes());
        self.write_free_counts(&mut raw, free_blocks, free_inodes)?;
        self.set_super_checksum(&mut raw);
        Ok(raw)
    }

    pub(super) fn set_super_checksum(&self, raw: &mut [u8; 1024]) {
        if self.has_metadata_checksums() {
            let crc = checksum::crc32c(!0, &raw[..0x3fc]);
            raw[0x3fc..].copy_from_slice(&crc.to_le_bytes());
        }
    }

    pub(super) fn clean_superblock(&self) -> [u8; 1024] {
        let mut raw = self.raw;
        raw[0x60..0x64].copy_from_slice(&(self.incompatible & !INCOMPAT_RECOVER).to_le_bytes());
        raw[0x64..0x68].copy_from_slice(&(self.read_only_compatible & !0x10000).to_le_bytes());
        let state = u16::from_le_bytes(raw[0x3a..0x3c].try_into().unwrap());
        raw[0x3a..0x3c].copy_from_slice(&(state | 1).to_le_bytes());
        self.set_super_checksum(&mut raw);
        raw
    }

    pub(super) fn block_offset(&self, block: u64) -> Result<u64> {
        if block < u64::from(self.first_data_block) || block >= self.blocks {
            return Err(Error::Corrupt);
        }
        block
            .checked_mul(u64::from(self.block_size))
            .ok_or(Error::Corrupt)
    }

    pub(super) fn descriptor_offset(&self, group: u32) -> Result<u64> {
        if group >= self.groups {
            return Err(Error::Corrupt);
        }
        Ok(
            (u64::from(self.first_data_block) + 1) * u64::from(self.block_size)
                + u64::from(group) * u64::from(self.descriptor_size),
        )
    }
}
