// SPDX-License-Identifier: MPL-2.0

//! External attribute-block integrity and shared ownership.

use alloc::{vec, vec::Vec};

mod entry;

use entry::{pack_entries, visit_entries};
use io_util::{ReadAt, checksum};

use crate::{Error, Ext4, Geometry, Inode, Result, extent::EXTENTS_FLAG, u16_at, u32_at};

/// One owned extended attribute, with its on-disk namespace index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attribute {
    pub(super) index: u8,
    pub(super) name: Vec<u8>,
    pub(super) value: Vec<u8>,
}

impl Attribute {
    pub fn index(&self) -> u8 {
        self.index
    }
    pub fn name(&self) -> &[u8] {
        &self.name
    }
    pub fn value(&self) -> &[u8] {
        &self.value
    }
    pub(super) fn key(&self) -> (u8, usize, &[u8]) {
        (self.index, self.name.len(), &self.name)
    }
}

pub(super) struct AttributeBlock {
    pub(super) physical: u64,
    pub(super) raw: Vec<u8>,
    pub(super) references: u32,
}

impl<D: ReadAt> Ext4<D> {
    /// Reads inline and external attributes, rejecting duplicate keys across stores.
    pub fn attributes(&self, inode: &Inode) -> Result<Vec<Attribute>> {
        let mut attributes = Vec::new();
        let mut append = |index, name: &[u8], value: &[u8]| {
            attributes.push(Attribute {
                index,
                name: name.to_vec(),
                value: value.to_vec(),
            });
            Ok(())
        };
        if inode.raw.len() > 128 {
            let begin = 128 + usize::from(u16_at(&inode.raw, 128)?);
            if let Some(raw) = inode.raw.get(begin..) {
                if raw.len() >= 4 && u32_at(raw, 0)? == 0xea02_0000 {
                    visit_entries(&inode.raw, begin + 4, begin + 4, false, &mut append)?;
                }
            } else {
                return Err(Error::Corrupt);
            }
        }
        if let Some(block) = self.external_attribute_block(inode)? {
            visit_entries(&block.raw, 32, 0, true, &mut append)?;
        }
        attributes.sort_unstable_by(|left, right| left.key().cmp(&right.key()));
        if attributes
            .windows(2)
            .any(|pair| pair[0].key() == pair[1].key())
        {
            return Err(Error::Corrupt);
        }
        Ok(attributes)
    }

    pub(super) fn check_reclaimable(&self, inode: &Inode) -> Result<()> {
        if inode.is_special() {
            if inode.has_data_allocations(&self.geometry)? {
                return Err(Error::Corrupt);
            }
        } else if !matches!(inode.mode() & 0xf000, 0x8000 | 0x4000 | 0xa000)
            || (u32_at(&inode.raw, 32)? & EXTENTS_FLAG == 0
                && !inode.is_inline_symlink(&self.geometry)?)
        {
            return Err(Error::Unsupported);
        }
        self.external_attribute_block(inode)?;
        Ok(())
    }

    pub(super) fn external_attribute_block(&self, inode: &Inode) -> Result<Option<AttributeBlock>> {
        let physical = inode.attribute_block_number()?;
        if physical == 0 {
            return Ok(None);
        }
        self.check_allocated_block(physical)?;
        let raw = self.read_block(physical)?;
        let references = u32_at(&raw, 4)?;
        if u32_at(&raw, 0)? != 0xea02_0000
            || !(1..=1024).contains(&references)
            || u32_at(&raw, 8)? != 1
            || raw[20..32].iter().any(|byte| *byte != 0)
            || (self.geometry.has_metadata_checksums()
                && block_checksum(&self.geometry, physical, &raw) != u32_at(&raw, 16)?)
        {
            return Err(Error::Corrupt);
        }
        visit_entries(&raw, 32, 0, true, &mut |_, _, _| Ok(()))?;
        Ok(Some(AttributeBlock {
            physical,
            raw,
            references,
        }))
    }
}

pub(super) fn block_checksum(geometry: &Geometry, physical: u64, raw: &[u8]) -> u32 {
    let seed = checksum::crc32c(geometry.checksum_seed(), &physical.to_le_bytes());
    let crc = checksum::crc32c(seed, &raw[..16]);
    let crc = checksum::crc32c(crc, &[0; 4]);
    checksum::crc32c(crc, &raw[20..])
}

impl Inode {
    pub(super) fn attribute_block_number(&self) -> Result<u64> {
        Ok(u64::from(u32_at(&self.raw, 104)?) | (u64::from(u16_at(&self.raw, 118)?) << 32))
    }
}

pub(super) fn encode_attributes(
    inode: &mut Inode,
    geometry: &Geometry,
    mut attributes: Vec<Attribute>,
) -> Result<Option<Vec<u8>>> {
    if inode.raw.len() > 128 {
        let begin = 128 + usize::from(u16_at(&inode.raw, 128)?);
        let body = inode.raw.get_mut(begin..).ok_or(Error::Corrupt)?;
        body.fill(0);
        if body.len() >= 4 {
            let packed = pack_entries(body, 4, 4, attributes, false)?;
            if packed.count > 0 {
                body[..4].copy_from_slice(&0xea02_0000u32.to_le_bytes());
            }
            attributes = packed.remaining;
        }
    }
    if attributes.is_empty() {
        return Ok(None);
    }
    let mut raw = vec![0; geometry.block_size() as usize];
    raw[..4].copy_from_slice(&0xea02_0000u32.to_le_bytes());
    raw[4..8].copy_from_slice(&1u32.to_le_bytes());
    raw[8..12].copy_from_slice(&1u32.to_le_bytes());
    let packed = pack_entries(&mut raw, 32, 0, attributes, true)?;
    if !packed.remaining.is_empty() {
        return Err(Error::NoSpace);
    }
    raw[12..16].copy_from_slice(&packed.hash.to_le_bytes());
    Ok(Some(raw))
}
