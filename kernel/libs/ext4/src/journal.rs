// SPDX-License-Identifier: MPL-2.0

//! JBD2 journal records and checksum validation.
//!
//! The wire format follows the Linux ext4 journal documentation.

use alloc::vec::Vec;

use io_util::{ReadAt, checksum};

use super::{Error, Ext4, Inode, Result, bytes_at};

mod write;

const JOURNAL_MAGIC: u32 = 0xc03b_3998;
const FEATURE_CHECKSUM: u32 = 1;
const FEATURE_64BIT: u32 = 2;
const FEATURE_CSUM_V2: u32 = 8;
const FEATURE_CSUM_V3: u32 = 16;

/// A journal checksum format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalChecksum {
    None,
    TransactionCrc32,
    BlockCrc32c16,
    BlockCrc32c32,
}

/// Checked internal journal geometry and checksum mode.
#[derive(Clone, Debug)]
pub struct Journal {
    pub(super) inode: Inode,
    raw_super: [u8; 1024],
    block_size: u32,
    blocks: u32,
    first: u32,
    start: u32,
    sequence: u32,
    checksum_seed: u32,
    checksum: JournalChecksum,
    has_64bit: bool,
}

/// One descriptor's metadata-block destination and integrity check.
#[derive(Clone, Debug)]
pub struct JournalTag {
    block: u64,
    flags: u32,
    checksum: u32,
}

impl JournalTag {
    pub fn block(&self) -> u64 {
        self.block
    }
    pub fn is_escaped(&self) -> bool {
        self.flags & 1 != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & 4 != 0
    }
}

impl<D: ReadAt> Ext4<D> {
    /// Reads and validates the internal journal superblock.
    pub fn journal(&self) -> Result<Option<Journal>> {
        let number = self.geometry.journal_inode();
        if number == 0 {
            return Ok(None);
        }
        let inode = if self.geometry.needs_recovery() {
            // The allocation bitmap and its descriptor can be checkpointed separately.
            // Journal bootstrap cannot depend on those records agreeing before replay.
            let group = self.group((number - 1) / self.geometry.inodes_per_group())?;
            self.read_inode_record(number, &group)?
        } else {
            self.inode(number)?
        };
        let mut raw = [0; 1024];
        if self.read_at(&inode, 0, &mut raw)? != raw.len() {
            return Err(Error::Corrupt);
        }
        if be32(&raw, 0)? != JOURNAL_MAGIC || be32(&raw, 4)? != 4 {
            return Err(Error::Corrupt);
        }
        let block_size = be32(&raw, 12)?;
        let blocks = be32(&raw, 16)?;
        let first = be32(&raw, 20)?;
        let sequence = be32(&raw, 24)?;
        let start = be32(&raw, 28)?;
        let compatible = be32(&raw, 36)?;
        let incompatible = be32(&raw, 40)?;
        if compatible & !FEATURE_CHECKSUM != 0 || incompatible & !31 != 0 || be32(&raw, 44)? != 0 {
            return Err(Error::Unsupported);
        }
        let checksum = match (
            compatible & FEATURE_CHECKSUM != 0,
            incompatible & (FEATURE_CSUM_V2 | FEATURE_CSUM_V3),
        ) {
            (false, 0) => JournalChecksum::None,
            (true, 0) => JournalChecksum::TransactionCrc32,
            (false, FEATURE_CSUM_V2) => JournalChecksum::BlockCrc32c16,
            (false, FEATURE_CSUM_V3) => JournalChecksum::BlockCrc32c32,
            _ => return Err(Error::Corrupt),
        };
        if block_size != self.geometry.block_size()
            || blocks < 3
            || u64::from(blocks) * u64::from(block_size) > inode.size()
            || first == 0
            || first >= blocks
            || (start != 0 && !(first..blocks).contains(&start))
            || be32(&raw, 64)? != 1
            || be32(&raw, 32)? != 0
        {
            return Err(Error::Corrupt);
        }
        if incompatible & 4 != 0 && checksum == JournalChecksum::None {
            return Err(Error::Corrupt);
        }
        let checksum_seed = checksum::crc32c(!0, &raw[48..64]);
        if matches!(
            checksum,
            JournalChecksum::BlockCrc32c16 | JournalChecksum::BlockCrc32c32
        ) {
            if raw[80] != 4 {
                return Err(Error::Unsupported);
            }
            let expected = be32(&raw, 252)?;
            raw[252..256].fill(0);
            if checksum::crc32c(!0, &raw) != expected {
                return Err(Error::Corrupt);
            }
            raw[252..256].copy_from_slice(&expected.to_be_bytes());
        }
        Ok(Some(Journal {
            inode,
            raw_super: raw,
            block_size,
            blocks,
            first,
            start,
            sequence,
            checksum_seed,
            checksum,
            has_64bit: incompatible & FEATURE_64BIT != 0,
        }))
    }

    /// Reads a journal block without permitting holes or unwritten extents.
    pub fn read_journal_block(&self, journal: &Journal, logical: u32) -> Result<Vec<u8>> {
        if logical >= journal.blocks {
            return Err(Error::Corrupt);
        }
        let physical = self
            .file_block(&journal.inode, logical)?
            .ok_or(Error::Corrupt)?;
        self.read_block(physical)
    }
}

impl Journal {
    pub fn first_block(&self) -> u32 {
        self.first
    }
    pub fn blocks(&self) -> u32 {
        self.blocks
    }
    pub fn start(&self) -> u32 {
        self.start
    }
    pub fn sequence(&self) -> u32 {
        self.sequence
    }
    pub fn checksum_mode(&self) -> JournalChecksum {
        self.checksum
    }
    pub fn uuid(&self) -> [u8; 16] {
        bytes_at(&self.raw_super, 48).unwrap()
    }

    pub(super) fn clean_superblock(&self, sequence: u32) -> [u8; 1024] {
        self.superblock(0, sequence)
    }

    fn superblock(&self, start: u32, sequence: u32) -> [u8; 1024] {
        let mut raw = self.raw_super;
        raw[24..28].copy_from_slice(&sequence.to_be_bytes());
        raw[28..32].copy_from_slice(&start.to_be_bytes());
        if matches!(
            self.checksum,
            JournalChecksum::BlockCrc32c16 | JournalChecksum::BlockCrc32c32
        ) {
            raw[252..256].fill(0);
            let crc = checksum::crc32c(!0, &raw);
            raw[252..256].copy_from_slice(&crc.to_be_bytes());
        }
        raw
    }

    /// Advances within the journal ring.
    pub fn next_block(&self, block: u32) -> Result<u32> {
        if !(self.first..self.blocks).contains(&block) {
            return Err(Error::Corrupt);
        }
        Ok(if block + 1 == self.blocks {
            self.first
        } else {
            block + 1
        })
    }

    /// Parses a descriptor after checking its sequence and metadata checksum.
    pub fn descriptor(&self, raw: &[u8], sequence: u32) -> Result<Vec<JournalTag>> {
        self.check_header(raw, 1, sequence)?;
        let end = self.metadata_end(raw)?;
        let mut offset: usize = 12;
        let mut tags = Vec::new();
        let tag_size = self.tag_size();
        loop {
            let tag_end = offset.checked_add(tag_size).ok_or(Error::Corrupt)?;
            if tag_end > end {
                return Err(Error::Corrupt);
            }
            let (flags, checksum) = if self.checksum == JournalChecksum::BlockCrc32c32 {
                (be32(raw, offset + 4)?, be32(raw, offset + 12)?)
            } else {
                (
                    u32::from(be16(raw, offset + 6)?),
                    u32::from(be16(raw, offset + 4)?),
                )
            };
            if flags & !15 != 0 {
                return Err(Error::Corrupt);
            }
            if flags & 4 != 0 {
                return Err(Error::Unsupported);
            }
            if !self.has_64bit
                && self.checksum == JournalChecksum::BlockCrc32c32
                && be32(raw, offset + 8)? != 0
            {
                return Err(Error::Corrupt);
            }
            let high = if self.has_64bit {
                u64::from(be32(raw, offset + 8)?) << 32
            } else {
                0
            };
            let block = u64::from(be32(raw, offset)?) | high;
            offset = tag_end;
            if flags & 2 == 0 {
                let uuid_end = offset.checked_add(16).ok_or(Error::Corrupt)?;
                // Linux can leave the tag UUID unset for an internal journal.
                if uuid_end > end {
                    return Err(Error::Corrupt);
                }
                offset = uuid_end;
            }
            tags.push(JournalTag {
                block,
                flags,
                checksum,
            });
            if flags & 8 != 0 {
                return Ok(tags);
            }
        }
    }

    /// Checks a journalled data block before removing any escape marker.
    pub fn check_data(&self, tag: &JournalTag, sequence: u32, raw: &[u8]) -> Result<()> {
        if raw.len() != self.block_size as usize {
            return Err(Error::Corrupt);
        }
        let checksum = checksum::crc32c(self.checksum_seed, &sequence.to_be_bytes());
        let checksum = checksum::crc32c(checksum, raw);
        match self.checksum {
            JournalChecksum::BlockCrc32c16 if checksum as u16 != tag.checksum as u16 => {
                Err(Error::Corrupt)
            }
            JournalChecksum::BlockCrc32c32 if checksum != tag.checksum => Err(Error::Corrupt),
            _ => Ok(()),
        }
    }

    /// Checks the transaction's commit record.
    pub fn check_commit(&self, raw: &[u8], sequence: u32, transaction_crc: u32) -> Result<()> {
        self.check_header(raw, 2, sequence)?;
        match self.checksum {
            JournalChecksum::None => Ok(()),
            JournalChecksum::TransactionCrc32 => {
                if raw[12] != 1 || raw[13] != 4 || be32(raw, 16)? != transaction_crc {
                    return Err(Error::Corrupt);
                }
                Ok(())
            }
            JournalChecksum::BlockCrc32c16 | JournalChecksum::BlockCrc32c32 => {
                let crc = checksum::crc32c(self.checksum_seed, &raw[..16]);
                let crc = checksum::crc32c(crc, &[0; 4]);
                if checksum::crc32c(crc, &raw[20..]) != be32(raw, 16)? {
                    return Err(Error::Corrupt);
                }
                Ok(())
            }
        }
    }

    /// Parses checked block revocations for one transaction.
    pub fn revocations(&self, raw: &[u8], sequence: u32) -> Result<Vec<u64>> {
        self.check_header(raw, 5, sequence)?;
        let end = self.metadata_end(raw)?;
        let count = be32(raw, 12)? as usize;
        let width = if self.has_64bit { 8 } else { 4 };
        if count < 16 || count > end || !(count - 16).is_multiple_of(width) {
            return Err(Error::Corrupt);
        }
        let mut blocks = Vec::new();
        for offset in (16..count).step_by(width) {
            let block = if self.has_64bit {
                u64::from_be_bytes(bytes_at(raw, offset)?)
            } else {
                u64::from(be32(raw, offset)?)
            };
            blocks.push(block);
        }
        Ok(blocks)
    }

    fn check_header(&self, raw: &[u8], block_type: u32, sequence: u32) -> Result<()> {
        if raw.len() != self.block_size as usize
            || be32(raw, 0)? != JOURNAL_MAGIC
            || be32(raw, 4)? != block_type
            || be32(raw, 8)? != sequence
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn tag_size(&self) -> usize {
        if self.checksum == JournalChecksum::BlockCrc32c32 {
            return 16;
        }
        let base = if self.has_64bit { 12 } else { 8 };
        // Linux's journal_tag_bytes reserves two extra bytes in the v2 format.
        base + if self.checksum == JournalChecksum::BlockCrc32c16 {
            2
        } else {
            0
        }
    }

    fn metadata_end(&self, raw: &[u8]) -> Result<usize> {
        if !matches!(
            self.checksum,
            JournalChecksum::BlockCrc32c16 | JournalChecksum::BlockCrc32c32
        ) {
            return Ok(raw.len());
        }
        let end = raw.len().checked_sub(4).ok_or(Error::Corrupt)?;
        let expected = be32(raw, end)?;
        let crc = checksum::crc32c(self.checksum_seed, &raw[..end]);
        if checksum::crc32c(crc, &[0; 4]) != expected {
            return Err(Error::Corrupt);
        }
        Ok(end)
    }
}

pub(super) fn record_header(raw: &[u8]) -> Result<(u32, u32)> {
    if be32(raw, 0)? != JOURNAL_MAGIC {
        return Err(Error::Corrupt);
    }
    Ok((be32(raw, 4)?, be32(raw, 8)?))
}

fn be16(raw: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(bytes_at(raw, offset)?))
}
fn be32(raw: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(bytes_at(raw, offset)?))
}
