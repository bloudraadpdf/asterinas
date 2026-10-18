// SPDX-License-Identifier: MPL-2.0

//! Checked HTree nodes and bounded search paths.

use alloc::{vec, vec::Vec};

use io_util::{ReadAt, checksum};

use crate::{Error, Ext4, Geometry, Inode, Result, u16_at, u32_at};

pub(crate) const INDEX_FLAG: u32 = 0x1000;

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    pub hash: u32,
    pub block: u32,
}

pub(crate) struct Node {
    pub logical: u32,
    pub raw: Vec<u8>,
    pub entries: Vec<Entry>,
    pub limit: usize,
    pub selected: usize,
}

impl Node {
    pub(crate) fn parse(
        geometry: &Geometry,
        inode: &Inode,
        logical: u32,
        raw: Vec<u8>,
    ) -> Result<Self> {
        let offset = if logical == 0 {
            if u32_at(&raw, 0)? != inode.number()
                || u16_at(&raw, 4)? != 12
                || raw[6] != 1
                || raw[8] != b'.'
                || u32_at(&raw, 12)? == 0
                || usize::from(u16_at(&raw, 16)?) != raw.len() - 12
                || raw[18] != 2
                || &raw[20..22] != b".."
                || u32_at(&raw, 24)? != 0
                || raw[29] != 8
                || raw[31] != 0
            {
                return Err(Error::Corrupt);
            }
            if raw[28] > 5 || raw[30] > 1 {
                return Err(Error::Unsupported);
            }
            32
        } else {
            if u32_at(&raw, 0)? != 0
                || usize::from(u16_at(&raw, 4)?) != raw.len()
                || u16_at(&raw, 6)? != 0
            {
                return Err(Error::Corrupt);
            }
            8
        };
        let limit = usize::from(u16_at(&raw, offset)?);
        let count = usize::from(u16_at(&raw, offset + 2)?);
        let tail_size = if geometry.has_metadata_checksums() {
            8
        } else {
            0
        };
        if limit != (raw.len() - offset - tail_size) / 8 || count == 0 || count > limit {
            return Err(Error::Corrupt);
        }
        let mut entries = Vec::with_capacity(count);
        let mut last = 0;
        for index in 0..count {
            let hash = if index == 0 {
                0
            } else {
                u32_at(&raw, offset + index * 8)?
            };
            let block = u32_at(&raw, offset + index * 8 + 4)?;
            if hash < last
                || block == 0
                || block == logical
                || u64::from(block) >= inode.size() / u64::from(geometry.block_size())
            {
                return Err(Error::Corrupt);
            }
            last = hash;
            entries.push(Entry { hash, block });
        }
        let mut children: Vec<_> = entries.iter().map(|entry: &Entry| entry.block).collect();
        children.sort_unstable();
        if children.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Error::Corrupt);
        }
        let node = Self {
            logical,
            raw,
            entries,
            limit,
            selected: 0,
        };
        if geometry.has_metadata_checksums() {
            let tail = offset + limit * 8;
            if u32_at(&node.raw, tail)? != 0
                || node.checksum(geometry, inode)? != u32_at(&node.raw, tail + 4)?
            {
                return Err(Error::Corrupt);
            }
        }
        Ok(node)
    }

    pub(crate) fn interior(geometry: &Geometry, logical: u32, entries: Vec<Entry>) -> Self {
        let mut raw = vec![0; geometry.block_size() as usize];
        raw[4..6].copy_from_slice(&(geometry.block_size() as u16).to_le_bytes());
        let tail = if geometry.has_metadata_checksums() {
            8
        } else {
            0
        };
        let limit = (raw.len() - 8 - tail) / 8;
        Self {
            logical,
            raw,
            entries,
            limit,
            selected: 0,
        }
    }

    pub(crate) fn encode(&mut self, geometry: &Geometry, inode: &Inode) -> Result<()> {
        let offset = self.offset();
        if self.entries.is_empty() || self.entries.len() > self.limit {
            return Err(Error::Corrupt);
        }
        self.raw[offset..].fill(0);
        for (index, entry) in self.entries.iter().enumerate() {
            let position = offset + index * 8;
            self.raw[position..position + 4].copy_from_slice(&entry.hash.to_le_bytes());
            self.raw[position + 4..position + 8].copy_from_slice(&entry.block.to_le_bytes());
        }
        self.raw[offset..offset + 2].copy_from_slice(&(self.limit as u16).to_le_bytes());
        self.raw[offset + 2..offset + 4]
            .copy_from_slice(&(self.entries.len() as u16).to_le_bytes());
        if geometry.has_metadata_checksums() {
            let crc = self.checksum(geometry, inode)?;
            let tail = offset + self.limit * 8;
            self.raw[tail + 4..tail + 8].copy_from_slice(&crc.to_le_bytes());
        }
        Ok(())
    }

    fn offset(&self) -> usize {
        if self.logical == 0 { 32 } else { 8 }
    }

    fn checksum(&self, geometry: &Geometry, inode: &Inode) -> Result<u32> {
        let seed = checksum::crc32c(geometry.checksum_seed(), &inode.number().to_le_bytes());
        let seed = checksum::crc32c(seed, &inode.generation().to_le_bytes());
        let end = self.offset() + self.entries.len() * 8;
        let tail = self.offset() + self.limit * 8;
        let crc = checksum::crc32c(seed, &self.raw[..end]);
        let crc = checksum::crc32c(crc, &self.raw[tail..tail + 4]);
        Ok(checksum::crc32c(crc, &[0; 4]))
    }
}

pub(crate) struct Path {
    pub nodes: Vec<Node>,
    pub hash: u32,
}

impl Path {
    pub(crate) fn leaf(&self) -> u32 {
        let last = self.nodes.last().unwrap();
        last.entries[last.selected].block
    }

    pub(crate) fn next<D: ReadAt>(&mut self, fs: &Ext4<D>, inode: &Inode) -> Result<bool> {
        for level in (0..self.nodes.len()).rev() {
            let node = &mut self.nodes[level];
            if node.selected + 1 == node.entries.len() {
                continue;
            }
            node.selected += 1;
            if node.entries[node.selected].hash & !1 != self.hash {
                return Ok(false);
            }
            for child in level + 1..self.nodes.len() {
                let parent = &self.nodes[child - 1];
                let logical = parent.entries[parent.selected].block;
                self.nodes[child] = fs.index_node(inode, logical)?;
            }
            return Ok(true);
        }
        Ok(false)
    }
}

impl<D: ReadAt> Ext4<D> {
    pub(crate) fn index_node(&self, inode: &Inode, logical: u32) -> Result<Node> {
        let physical = self.file_block(inode, logical)?.ok_or(Error::Corrupt)?;
        Node::parse(&self.geometry, inode, logical, self.read_block(physical)?)
    }

    pub(crate) fn index_path(&self, inode: &Inode, name: &[u8]) -> Result<Path> {
        let root = self.index_node(inode, 0)?;
        let hash = super::hash::name_hash(&self.geometry, root.raw[28], name)?;
        let levels = usize::from(root.raw[30]);
        let mut nodes = vec![root];
        for level in 0..=levels {
            let node = &mut nodes[level];
            node.selected = node
                .entries
                .partition_point(|entry| entry.hash <= hash)
                .saturating_sub(1);
            let child = node.entries[node.selected].block;
            if nodes.iter().any(|node| node.logical == child) {
                return Err(Error::Corrupt);
            }
            if level != levels {
                nodes.push(self.index_node(inode, child)?);
            }
        }
        Ok(Path { nodes, hash })
    }
}
