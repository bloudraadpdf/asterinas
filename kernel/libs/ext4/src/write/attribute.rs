// SPDX-License-Identifier: MPL-2.0

//! Atomic attribute updates and shared external-block copy-on-write.

use alloc::vec::Vec;

use io_util::{ReadAt, WriteAt};

use super::{Timestamp, Transaction, WritableExt4};
use crate::{Attribute, Error, Inode, Result, attribute, u32_at};

/// Whether an attribute must exist before the update.
#[derive(Clone, Copy, Debug)]
pub enum AttributeMode {
    Create,
    Replace,
    Upsert,
}

impl<D: WriteAt> WritableExt4<D> {
    /// Creates or replaces a byte-valued attribute in one metadata transaction.
    pub fn set_attribute(
        &mut self,
        number: u32,
        index: u8,
        name: &[u8],
        value: &[u8],
        mode: AttributeMode,
        changed: Timestamp,
    ) -> Result<()> {
        if name.len() > 255 || name.contains(&0) || (index == 0 && name.is_empty()) {
            return Err(Error::InvalidInput);
        }
        if value.len() > self.geometry().block_size() as usize {
            return Err(Error::NoSpace);
        }
        self.update_attributes(number, changed, |attributes| {
            let position = attributes
                .iter()
                .position(|attribute| attribute.index == index && attribute.name == name);
            match (mode, position) {
                (AttributeMode::Create, Some(_)) => return Err(Error::AlreadyExists),
                (AttributeMode::Replace, None) => return Err(Error::NoAttribute),
                _ => (),
            }
            let attribute = Attribute {
                index,
                name: name.to_vec(),
                value: value.to_vec(),
            };
            if let Some(position) = position {
                attributes[position] = attribute;
            } else {
                attributes.push(attribute);
            }
            Ok(())
        })
    }

    /// Removes an attribute in one metadata transaction.
    pub fn remove_attribute(
        &mut self,
        number: u32,
        index: u8,
        name: &[u8],
        changed: Timestamp,
    ) -> Result<()> {
        self.update_attributes(number, changed, |attributes| {
            let position = attributes
                .iter()
                .position(|attribute| attribute.index == index && attribute.name == name)
                .ok_or(Error::NoAttribute)?;
            attributes.remove(position);
            Ok(())
        })
    }

    fn update_attributes(
        &mut self,
        number: u32,
        changed: Timestamp,
        change: impl FnOnce(&mut Vec<Attribute>) -> Result<()>,
    ) -> Result<()> {
        self.update(|transaction| {
            let mut inode = transaction.metadata_inode(number)?;
            if u32_at(&inode.raw, 32)? & 0x30 != 0 {
                return Err(Error::PermissionDenied);
            }
            transaction.check_attribute_block(&inode)?;
            let mut attributes = transaction.view().attributes(&inode)?;
            change(&mut attributes)?;
            attributes.sort_unstable_by(|left, right| left.key().cmp(&right.key()));
            transaction.store_attributes(&mut inode, attributes)?;
            changed.store(&mut inode, 12, 132)?;
            transaction.store_inode(inode)
        })
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    fn store_attributes(&mut self, inode: &mut Inode, attributes: Vec<Attribute>) -> Result<()> {
        if !attributes.is_empty() && u32_at(&self.geometry.raw, 0x5c)? & 8 == 0 {
            let mut raw = self.geometry.raw;
            let compatible = u32_at(&raw, 0x5c)? | 8;
            raw[0x5c..0x60].copy_from_slice(&compatible.to_le_bytes());
            self.store_superblock(raw)?;
        }
        let old = self.view().external_attribute_block(inode)?;
        let raw = attribute::encode_attributes(inode, &self.geometry, attributes)?;
        let Some(mut raw) = raw else {
            return self.release_attributes(inode);
        };
        if let Some(old) = &old
            && old.raw[..4] == raw[..4]
            && old.raw[8..16] == raw[8..16]
            && old.raw[20..] == raw[20..]
        {
            return Ok(());
        }
        let physical = if let Some(old) = old.filter(|block| block.references == 1) {
            old.physical
        } else {
            let physical = self.allocate_block(None)?;
            self.release_attributes(inode)?;
            super::extent::adjust_inode_blocks(inode, 1, self.geometry.block_size())?;
            physical
        };
        if self.geometry.has_metadata_checksums() {
            let crc = attribute::block_checksum(&self.geometry, physical, &raw);
            raw[16..20].copy_from_slice(&crc.to_le_bytes());
        }
        self.metadata_block(physical)?.copy_from_slice(&raw);
        inode.raw[104..108].copy_from_slice(&(physical as u32).to_le_bytes());
        inode.raw[118..120].copy_from_slice(&((physical >> 32) as u16).to_le_bytes());

        Ok(())
    }
    pub(super) fn release_attributes(&mut self, inode: &mut Inode) -> Result<()> {
        let Some(mut block) = self.view().external_attribute_block(inode)? else {
            return Ok(());
        };
        if self.reserved.contains(block.physical) {
            return Err(Error::Corrupt);
        }
        if block.references == 1 {
            self.free_blocks(block.physical, 1)?;
        } else {
            block.raw[4..8].copy_from_slice(&(block.references - 1).to_le_bytes());
            if self.geometry.has_metadata_checksums() {
                let crc = attribute::block_checksum(&self.geometry, block.physical, &block.raw);
                block.raw[16..20].copy_from_slice(&crc.to_le_bytes());
            }
            self.metadata_block(block.physical)?
                .copy_from_slice(&block.raw);
        }
        super::extent::adjust_inode_blocks(inode, -1, self.geometry.block_size())?;
        inode.raw[104..108].fill(0);
        inode.raw[118..120].fill(0);
        Ok(())
    }
}
