// SPDX-License-Identifier: MPL-2.0

//! Validated extent-tree nodes shared by lookup and mutation.

use alloc::{vec, vec::Vec};
use core::ops::Range;

use io_util::checksum;

use crate::{Error, Geometry, Inode, Result, u16_at, u32_at};

const MAGIC: u16 = 0xf30a;
const HEADER_SIZE: usize = 12;
const RECORD_SIZE: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentKind {
    Written,
    Unwritten,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Extent {
    pub(crate) logical: u32,
    pub(crate) physical: u64,
    pub(crate) blocks: u32,
    pub(crate) kind: ExtentKind,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Record {
    Leaf(Extent),
    Index { logical: u32, physical: u64 },
}

impl Record {
    pub(crate) fn logical(self) -> u32 {
        match self {
            Self::Leaf(extent) => extent.logical,
            Self::Index { logical, .. } => logical,
        }
    }
}

pub(crate) struct Node {
    pub(crate) records: Vec<Record>,
    pub(crate) capacity: u16,
    pub(crate) depth: u16,
    generation: u32,
}

impl Node {
    pub(crate) fn root(inode: &Inode, geometry: &Geometry) -> Result<Self> {
        Self::parse(&inode.raw[40..100], inode, geometry, None)
    }

    pub(crate) fn external(
        raw: &[u8],
        inode: &Inode,
        geometry: &Geometry,
        depth: u16,
        bounds: Range<u64>,
    ) -> Result<Self> {
        let node = Self::parse(raw, inode, geometry, Some(bounds))?;
        if node.depth != depth {
            return Err(Error::Corrupt);
        }
        Ok(node)
    }

    fn parse(
        raw: &[u8],
        inode: &Inode,
        geometry: &Geometry,
        bounds: Option<Range<u64>>,
    ) -> Result<Self> {
        if u16_at(raw, 0)? != MAGIC {
            return Err(Error::Corrupt);
        }
        let count = usize::from(u16_at(raw, 2)?);
        let capacity = u16_at(raw, 4)?;
        let depth = u16_at(raw, 6)?;
        let tail = HEADER_SIZE + usize::from(capacity) * RECORD_SIZE;
        let external = bounds.is_some();
        let checksum_size = if external && geometry.has_metadata_checksums() {
            4
        } else {
            0
        };
        if capacity == 0
            || count > usize::from(capacity)
            || tail
                .checked_add(checksum_size)
                .is_none_or(|end| end > raw.len())
            || depth > 5
            || (depth != 0 && count == 0)
        {
            return Err(Error::Corrupt);
        }
        if checksum_size != 0
            && checksum::crc32c(inode_seed(inode, geometry)?, &raw[..tail]) != u32_at(raw, tail)?
        {
            return Err(Error::Corrupt);
        }
        let mut records = Vec::with_capacity(count);
        let mut previous_end = 0;
        let upper = bounds
            .as_ref()
            .map_or(u64::from(u32::MAX) + 1, |range| range.end);
        for index in 0..count {
            let offset = HEADER_SIZE + index * RECORD_SIZE;
            let logical = u32_at(raw, offset)?;
            let start = u64::from(logical);
            if (index == 0 && bounds.as_ref().is_some_and(|range| start != range.start))
                || start < previous_end
                || start >= upper
            {
                return Err(Error::Corrupt);
            }
            let record = if depth == 0 {
                let encoded = u16_at(raw, offset + 4)?;
                let kind = if encoded > 32768 {
                    ExtentKind::Unwritten
                } else {
                    ExtentKind::Written
                };
                let blocks = u32::from(if encoded > 32768 {
                    encoded - 32768
                } else {
                    encoded
                });
                let physical = u64::from(u32_at(raw, offset + 8)?)
                    | (u64::from(u16_at(raw, offset + 6)?) << 32);
                let end = start + u64::from(blocks);
                if blocks == 0
                    || end > upper
                    || physical
                        .checked_add(u64::from(blocks))
                        .is_none_or(|end| end > geometry.blocks())
                {
                    return Err(Error::Corrupt);
                }
                geometry.block_offset(physical)?;
                previous_end = end;
                Record::Leaf(Extent {
                    logical,
                    physical,
                    blocks,
                    kind,
                })
            } else {
                let physical = u64::from(u32_at(raw, offset + 4)?)
                    | (u64::from(u16_at(raw, offset + 8)?) << 32);
                geometry.block_offset(physical)?;
                previous_end = start + 1;
                Record::Index { logical, physical }
            };
            records.push(record);
        }
        Ok(Self {
            records,
            capacity,
            depth,
            generation: u32_at(raw, 8)?,
        })
    }

    pub(crate) fn new(depth: u16, capacity: u16, records: Vec<Record>) -> Self {
        Self {
            records,
            capacity,
            depth,
            generation: 0,
        }
    }

    pub(crate) fn external_capacity(geometry: &Geometry) -> u16 {
        ((geometry.block_size() as usize
            - HEADER_SIZE
            - if geometry.has_metadata_checksums() {
                4
            } else {
                0
            })
            / RECORD_SIZE) as u16
    }

    pub(crate) fn serialize(
        &self,
        inode: &Inode,
        geometry: &Geometry,
        size: usize,
    ) -> Result<Vec<u8>> {
        let tail = HEADER_SIZE + usize::from(self.capacity) * RECORD_SIZE;
        let has_checksum = size != 60 && geometry.has_metadata_checksums();
        if self.records.len() > usize::from(self.capacity)
            || tail + if has_checksum { 4 } else { 0 } > size
            || self.depth > 5
        {
            return Err(Error::Corrupt);
        }
        let mut raw = vec![0; size];
        raw[..2].copy_from_slice(&MAGIC.to_le_bytes());
        raw[2..4].copy_from_slice(&(self.records.len() as u16).to_le_bytes());
        raw[4..6].copy_from_slice(&self.capacity.to_le_bytes());
        raw[6..8].copy_from_slice(&self.depth.to_le_bytes());
        raw[8..12].copy_from_slice(&self.generation.to_le_bytes());
        for (index, record) in self.records.iter().enumerate() {
            let offset = HEADER_SIZE + index * RECORD_SIZE;
            raw[offset..offset + 4].copy_from_slice(&record.logical().to_le_bytes());
            match *record {
                Record::Leaf(extent) => {
                    let maximum = if extent.kind == ExtentKind::Written {
                        32768
                    } else {
                        32767
                    };
                    if extent.blocks == 0 || extent.blocks > maximum || self.depth != 0 {
                        return Err(Error::Corrupt);
                    }
                    let encoded = extent.blocks as u16
                        + if extent.kind == ExtentKind::Unwritten {
                            32768
                        } else {
                            0
                        };
                    raw[offset + 4..offset + 6].copy_from_slice(&encoded.to_le_bytes());
                    raw[offset + 6..offset + 8]
                        .copy_from_slice(&((extent.physical >> 32) as u16).to_le_bytes());
                    raw[offset + 8..offset + 12]
                        .copy_from_slice(&(extent.physical as u32).to_le_bytes());
                }
                Record::Index { physical, .. } => {
                    if self.depth == 0 {
                        return Err(Error::Corrupt);
                    }
                    raw[offset + 4..offset + 8].copy_from_slice(&(physical as u32).to_le_bytes());
                    raw[offset + 8..offset + 10]
                        .copy_from_slice(&((physical >> 32) as u16).to_le_bytes());
                }
            }
        }
        if has_checksum {
            let crc = checksum::crc32c(inode_seed(inode, geometry)?, &raw[..tail]);
            raw[tail..tail + 4].copy_from_slice(&crc.to_le_bytes());
        }
        Ok(raw)
    }
}

fn inode_seed(inode: &Inode, geometry: &Geometry) -> Result<u32> {
    let crc = checksum::crc32c(geometry.checksum_seed(), &inode.number().to_le_bytes());
    Ok(checksum::crc32c(
        crc,
        &u32_at(&inode.raw, 100)?.to_le_bytes(),
    ))
}
