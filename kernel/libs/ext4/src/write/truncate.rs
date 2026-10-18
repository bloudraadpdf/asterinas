// SPDX-License-Identifier: MPL-2.0

//! Restartable truncation with journalled orphan ownership.

use io_util::{ReadAt, WriteAt};

use super::{Timestamp, Transaction, WritableExt4};
use crate::{Error, Inode, Result, extent::EXTENTS_FLAG, orphan::Location, u32_at};

impl<D: WriteAt> WritableExt4<D> {
    /// Changes a regular file's length, freeing excess extents in bounded transactions.
    pub fn truncate(&mut self, number: u32, size: u64, modified: Timestamp) -> Result<()> {
        let shrinking = self.update(|transaction| {
            let mut inode = transaction.mutable_inode(number)?;
            if inode.mode() & 0xf000 != 0x8000 {
                return Err(Error::InvalidInput);
            }
            if u32_at(&inode.raw, 32)? & 0x30 != 0 {
                return Err(Error::PermissionDenied);
            }
            let block_size = u64::from(transaction.geometry.block_size());
            if size > (u64::from(u32::MAX) + 1) * block_size {
                return Err(Error::InvalidInput);
            }
            if size < inode.size() {
                if u32_at(&inode.raw, 32)? & EXTENTS_FLAG == 0 {
                    return Err(Error::Unsupported);
                }
                transaction.add_orphan(&mut inode)?;
            }
            let shrinking = size < inode.size();
            if size > inode.size() {
                transaction.zero_tail(&inode, inode.size())?;
            }
            inode.set_size(size);
            modified.store(&mut inode, 16, 136)?;
            modified.store(&mut inode, 12, 132)?;
            transaction.store_inode(inode)?;
            Ok(shrinking)
        })?;
        if shrinking {
            self.finish_truncate(number)
        } else {
            Ok(())
        }
    }

    pub(crate) fn finish_truncate(&mut self, number: u32) -> Result<()> {
        self.run_cleanup(|session| {
            session.update(|transaction| {
                let inode = transaction.view().inode(number)?;
                transaction.zero_tail(&inode, inode.size())?;
                Ok(())
            })?;
            while session.update(|transaction| {
                let mut inode = transaction.view().inode(number)?;
                let changed = transaction.trim_file_tail(&mut inode)?;
                if changed {
                    transaction.store_inode(inode)?;
                }
                Ok(changed)
            })? {}
            session.update(|transaction| {
                let mut inode = transaction.view().inode(number)?;
                if inode.links() != 0 {
                    transaction.remove_orphan(&mut inode)?;
                    transaction.store_inode(inode)?;
                }
                Ok(())
            })
        })
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    pub(super) fn add_orphan(&mut self, inode: &mut Inode) -> Result<()> {
        let entries = self.view().orphans(self.limits.orphan_inodes)?;
        if entries.iter().any(|entry| entry.number == inode.number()) {
            return Ok(());
        }
        if entries.len() >= self.limits.orphan_inodes {
            return Err(Error::ResourceLimit);
        }
        if let Some(file) = self.view().orphan_file()? {
            let blocks = file.size() / u64::from(self.geometry.block_size());
            for logical in 0..blocks {
                let (block, mut raw) = self.view().orphan_block(&file, logical as u32)?;
                for offset in (0..raw.len() - 8).step_by(4) {
                    if u32_at(&raw, offset)? == 0 {
                        raw[offset..offset + 4].copy_from_slice(&inode.number().to_le_bytes());
                        return self.store_orphan_block(&file, block, raw);
                    }
                }
            }
        }
        let mut raw = self.geometry.raw;
        inode.raw[20..24].copy_from_slice(&self.geometry.last_orphan().to_le_bytes());
        raw[0xe8..0xec].copy_from_slice(&inode.number().to_le_bytes());
        self.store_superblock(raw)
    }

    pub(super) fn remove_orphan(&mut self, inode: &mut Inode) -> Result<()> {
        let entries = self.view().orphans(self.limits.orphan_inodes)?;
        let Some(entry) = entries.iter().find(|entry| entry.number == inode.number()) else {
            return Ok(());
        };
        match entry.location {
            Location::File { block, offset } => {
                let file = self.view().orphan_file()?.ok_or(Error::Corrupt)?;
                let mut raw = self.view().read_block(block)?;
                raw[offset..offset + 4].fill(0);
                self.store_orphan_block(&file, block, raw)?;
            }
            Location::List { previous } => {
                let next = u32_at(&inode.raw, 20)?;
                if let Some(previous) = previous {
                    let mut previous = self.view().inode(previous)?;
                    previous.raw[20..24].copy_from_slice(&next.to_le_bytes());
                    self.store_inode(previous)?;
                } else {
                    let mut raw = self.geometry.raw;
                    raw[0xe8..0xec].copy_from_slice(&next.to_le_bytes());
                    self.store_superblock(raw)?;
                }
                inode.raw[20..24].fill(0);
            }
        }
        Ok(())
    }

    fn store_orphan_block(
        &mut self,
        file: &Inode,
        block: u64,
        mut raw: alloc::vec::Vec<u8>,
    ) -> Result<()> {
        if self.geometry.has_metadata_checksums() {
            let crc = self.view().orphan_checksum(file, block, &raw)?;
            let tail = raw.len() - 4;
            raw[tail..].copy_from_slice(&crc.to_le_bytes());
        }
        self.metadata_block(block)?.copy_from_slice(&raw);
        Ok(())
    }
}
