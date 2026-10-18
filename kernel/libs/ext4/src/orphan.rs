// SPDX-License-Identifier: MPL-2.0

//! Checked orphan records retained until inode cleanup is durable.

use alloc::{collections::BTreeSet, vec::Vec};

use io_util::{ReadAt, checksum};

use crate::{Error, Ext4, Inode, Result, u32_at};

#[derive(Clone, Copy)]
pub(super) enum Location {
    File { block: u64, offset: usize },
    List { previous: Option<u32> },
}

pub(super) struct Orphan {
    pub(super) number: u32,
    pub(super) location: Location,
}

impl<D: ReadAt> Ext4<D> {
    pub(super) fn orphans(&self, limit: usize) -> Result<Vec<Orphan>> {
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        let mut push = |number, location| -> Result<()> {
            if !seen.insert(number) {
                return Err(Error::Corrupt);
            }
            if entries.len() >= limit {
                return Err(Error::ResourceLimit);
            }
            if number < u32_at(&self.geometry.raw, 0x54)?
                || number == self.geometry.journal_inode()
                || number == self.geometry.orphan_file_inode()
            {
                return Err(Error::Corrupt);
            }
            self.inode(number)?;
            entries.push(Orphan { number, location });
            Ok(())
        };
        let mut previous = None;
        let mut number = self.geometry.last_orphan();
        while number != 0 {
            push(number, Location::List { previous })?;
            previous = Some(number);
            number = u32_at(&self.inode(number)?.raw, 20)?;
        }
        if let Some(inode) = self.orphan_file()? {
            let blocks = inode.size() / u64::from(self.geometry.block_size());
            for logical in 0..blocks {
                let (block, raw) = self.orphan_block(&inode, logical as u32)?;
                for offset in (0..raw.len() - 8).step_by(4) {
                    let number = u32_at(&raw, offset)?;
                    if number != 0 {
                        push(number, Location::File { block, offset })?;
                    }
                }
            }
        }
        Ok(entries)
    }

    pub(super) fn check_empty_orphans(&self) -> Result<()> {
        if self.orphans(1)?.is_empty() {
            Ok(())
        } else {
            Err(Error::Unsupported)
        }
    }

    pub(super) fn orphan_file(&self) -> Result<Option<Inode>> {
        if !self.geometry.has_orphan_file() {
            return if self.geometry.orphan_cleanup_required() {
                Err(Error::Corrupt)
            } else {
                Ok(None)
            };
        }
        let inode = self.inode(self.geometry.orphan_file_inode())?;
        if inode.mode() & 0xf000 != 0x8000
            || !inode
                .size()
                .is_multiple_of(u64::from(self.geometry.block_size()))
        {
            return Err(Error::Corrupt);
        }
        if inode.size() > 8 << 20 {
            return Err(Error::ResourceLimit);
        }
        Ok(Some(inode))
    }

    pub(super) fn orphan_block(&self, inode: &Inode, logical: u32) -> Result<(u64, Vec<u8>)> {
        let physical = self.file_block(inode, logical)?.ok_or(Error::Corrupt)?;
        let raw = self.read_block(physical)?;
        let tail = raw.len() - 8;
        if u32_at(&raw, tail)? != 0x0b10_ca04
            || (self.geometry.has_metadata_checksums()
                && self.orphan_checksum(inode, physical, &raw)? != u32_at(&raw, tail + 4)?)
        {
            return Err(Error::Corrupt);
        }
        Ok((physical, raw))
    }

    pub(super) fn orphan_checksum(&self, inode: &Inode, physical: u64, raw: &[u8]) -> Result<u32> {
        let seed = checksum::crc32c(self.geometry.checksum_seed(), &inode.number().to_le_bytes());
        let seed = checksum::crc32c(seed, &u32_at(&inode.raw, 100)?.to_le_bytes());
        let seed = checksum::crc32c(seed, &physical.to_le_bytes());
        Ok(checksum::crc32c(seed, &raw[..raw.len() - 8]))
    }
}
