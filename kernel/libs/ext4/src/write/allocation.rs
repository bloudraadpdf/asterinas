// SPDX-License-Identifier: MPL-2.0

//! Checked allocation bitmaps and filesystem metadata reservations.

use alloc::{collections::BTreeSet, vec, vec::Vec};
use core::ops::Range;

use io_util::{ReadAt, checksum};

use super::{CreateMetadata, MountLimits, NewInodeKind, Transaction};
use crate::{
    Error, Ext4, Geometry, Group, Inode, Result, extent::Node, recovery::JournalMap, u16_at, u32_at,
};

const BLOCK_UNINITIALISED: u16 = 2;

pub(super) struct Reserved {
    ranges: Vec<Range<u64>>,
}

impl Reserved {
    pub(super) fn scan<D: ReadAt>(
        fs: &Ext4<D>,
        journal: Option<&JournalMap>,
        limits: MountLimits,
    ) -> Result<(Self, u64, u32)> {
        let geometry = &fs.geometry;
        if geometry.groups() > limits.allocation_groups {
            return Err(Error::ResourceLimit);
        }
        let mut ranges = Vec::new();
        let descriptor_blocks = (u64::from(geometry.groups())
            * u64::from(geometry.descriptor_size()))
        .div_ceil(u64::from(geometry.block_size()));
        let reserved_gdt = u64::from(u16_at(&geometry.raw, 0xce)?);
        let inode_blocks = (u64::from(geometry.inodes_per_group())
            * u64::from(geometry.inode_size()))
        .div_ceil(u64::from(geometry.block_size()));
        let mut free_blocks = 0u64;
        let mut free_inodes = 0u64;
        for number in 0..geometry.groups() {
            if geometry.has_backup_superblock(number) {
                let start = geometry.group_start(number);
                ranges.push(start..start + 1 + descriptor_blocks + reserved_gdt);
            }
            let group = fs.group(number)?;
            free_blocks = free_blocks
                .checked_add(u64::from(group.free_blocks()))
                .ok_or(Error::Corrupt)?;
            free_inodes = free_inodes
                .checked_add(u64::from(group.free_inodes()))
                .ok_or(Error::Corrupt)?;
            ranges.extend([
                group.block_bitmap()..group.block_bitmap() + 1,
                group.inode_bitmap()..group.inode_bitmap() + 1,
                group.inode_table()..group.inode_table() + inode_blocks,
            ]);
        }
        if let Some(journal) = journal {
            ranges.extend(journal.reserved_ranges());
        }
        if geometry.has_orphan_file() {
            let inode = fs.inode(geometry.orphan_file_inode())?;
            if inode.size() > 8 << 20 {
                return Err(Error::ResourceLimit);
            }
            let blocks = u32::try_from(inode.size() / u64::from(geometry.block_size()))
                .map_err(|_| Error::Corrupt)?;
            let mut nodes = BTreeSet::new();
            for logical in 0..blocks {
                let physical = fs
                    .file_block_with_nodes(&inode, logical, &mut |node| {
                        if !nodes.contains(&node) && nodes.len() >= limits.mapping_entries {
                            return Err(Error::ResourceLimit);
                        }
                        nodes.insert(node);
                        Ok(())
                    })?
                    .ok_or(Error::Corrupt)?;
                ranges.push(physical..physical + 1);
            }
            ranges.extend(nodes.into_iter().map(|node| node..node + 1));
        }
        ranges.sort_unstable_by_key(|range| range.start);
        let mut merged: Vec<Range<u64>> = Vec::new();
        for range in ranges {
            if range.start >= range.end || range.end > geometry.blocks() {
                return Err(Error::Corrupt);
            }
            if let Some(previous) = merged.last_mut() {
                if range.start < previous.end {
                    return Err(Error::Corrupt);
                }
                if range.start == previous.end {
                    previous.end = range.end;
                    continue;
                }
            }
            merged.push(range);
        }
        if free_blocks > geometry.blocks() - geometry.group_start(0)
            || free_inodes > u64::from(geometry.inodes())
        {
            return Err(Error::Corrupt);
        }
        Ok((Self { ranges: merged }, free_blocks, free_inodes as u32))
    }

    pub(super) fn contains(&self, block: u64) -> bool {
        let index = self.ranges.partition_point(|range| range.start <= block);
        index > 0 && self.ranges[index - 1].contains(&block)
    }

    fn within(&self, range: Range<u64>) -> impl Iterator<Item = Range<u64>> + '_ {
        let start = range.start;
        let end = range.end;
        let first = self.ranges.partition_point(|entry| entry.end <= start);
        self.ranges[first..]
            .iter()
            .take_while(move |entry| entry.start < end)
            .map(move |entry| entry.start.max(start)..entry.end.min(end))
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    pub(super) fn allocate_inode(
        &mut self,
        parent: u32,
        kind: NewInodeKind<'_>,
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        if metadata.permissions & !0x0fff != 0 {
            return Err(Error::InvalidInput);
        }
        if self.geometry.free_inodes() == 0 {
            return Err(Error::NoSpace);
        }
        let first = (parent - 1) / self.geometry.inodes_per_group();
        for step in 0..self.geometry.groups() {
            let number =
                ((u64::from(first) + u64::from(step)) % u64::from(self.geometry.groups())) as u32;
            let mut group = self.view().group(number)?;
            if group.free_inodes == 0 {
                continue;
            }
            let mut bitmap = self.inode_bitmap(number, &group)?;
            let start = u64::from(number) * u64::from(self.geometry.inodes_per_group());
            let valid = (u64::from(self.geometry.inodes()) - start)
                .min(u64::from(self.geometry.inodes_per_group())) as usize;
            let index = (0..valid)
                .find(|index| bitmap[*index / 8] & (1 << (*index % 8)) == 0)
                .ok_or(Error::Corrupt)?;
            let inode_number =
                u32::try_from(start + index as u64 + 1).map_err(|_| Error::Corrupt)?;
            if inode_number < u32_at(&self.geometry.raw, 0x54)? {
                return Err(Error::Corrupt);
            }
            bitmap[index / 8] |= 1 << (index % 8);
            let flags = group.flags & !1;
            group.raw[18..20].copy_from_slice(&flags.to_le_bytes());
            let unused = u32::from(u16_at(&group.raw, 28)?)
                | if group.raw.len() >= 64 {
                    u32::from(u16_at(&group.raw, 50)?) << 16
                } else {
                    0
                };
            let unused = unused.min(self.geometry.inodes_per_group() - index as u32 - 1);
            group.raw[28..30].copy_from_slice(&(unused as u16).to_le_bytes());
            if group.raw.len() >= 64 {
                group.raw[50..52].copy_from_slice(&((unused >> 16) as u16).to_le_bytes());
            }
            if matches!(kind, NewInodeKind::Directory) {
                adjust_directories(&mut group, 1, self.geometry.inodes_per_group())?;
            }
            self.store_inode_bitmap(number, group, bitmap, -1)?;
            let inode = self.new_inode(inode_number, kind, metadata)?;
            self.store_inode(inode)?;
            return self.view().inode(inode_number);
        }
        Err(Error::Corrupt)
    }

    pub(super) fn free_inode(&mut self, mut inode: Inode) -> Result<()> {
        let number = (inode.number() - 1) / self.geometry.inodes_per_group();
        let index = (inode.number() - 1) % self.geometry.inodes_per_group();
        let mut group = self.view().group(number)?;
        let mut bitmap = self.inode_bitmap(number, &group)?;
        let mask = 1 << (index % 8);
        if inode.links() != 0
            || u32_at(&inode.raw, 28)? != 0
            || u16_at(&inode.raw, 116)? != 0
            || bitmap[index as usize / 8] & mask == 0
        {
            return Err(Error::Corrupt);
        }
        bitmap[index as usize / 8] &= !mask;
        if inode.mode() & 0xf000 == 0x4000 {
            adjust_directories(&mut group, -1, self.geometry.inodes_per_group())?;
        }
        inode.raw.fill(0);
        self.store_inode(inode)?;
        self.store_inode_bitmap(number, group, bitmap, 1)
    }

    fn store_inode_bitmap(
        &mut self,
        number: u32,
        mut group: Group,
        bitmap: Vec<u8>,
        delta: i32,
    ) -> Result<()> {
        let free = group
            .free_inodes
            .checked_add_signed(delta)
            .ok_or(Error::Corrupt)?;
        group.raw[14..16].copy_from_slice(&(free as u16).to_le_bytes());
        if group.raw.len() >= 64 {
            group.raw[46..48].copy_from_slice(&((free >> 16) as u16).to_le_bytes());
        }
        if self.geometry.has_metadata_checksums() {
            let crc = checksum::crc32c(
                self.geometry.checksum_seed(),
                &bitmap[..self.geometry.inodes_per_group() as usize / 8],
            );
            group.raw[26..28].copy_from_slice(&(crc as u16).to_le_bytes());
            if group.raw.len() >= 64 {
                group.raw[58..60].copy_from_slice(&((crc >> 16) as u16).to_le_bytes());
            }
        }
        self.metadata_block(group.inode_bitmap)?
            .copy_from_slice(&bitmap);
        self.store_group(number, group)?;
        let free = self
            .geometry
            .free_inodes()
            .checked_add_signed(delta)
            .ok_or(Error::Corrupt)?;
        let mut raw = self.geometry.raw;
        self.geometry
            .write_free_counts(&mut raw, self.geometry.free_blocks(), free)?;
        if delta < 0 && self.geometry.inode_size() > 128 {
            raw[0x64] |= 0x40;
        }
        self.store_superblock(raw)
    }

    fn inode_bitmap(&self, number: u32, group: &Group) -> Result<Vec<u8>> {
        let geometry = &self.geometry;
        let uninitialised = group.flags & 1 != 0;
        let start = u64::from(number) * u64::from(geometry.inodes_per_group());
        let valid = (u64::from(geometry.inodes()) - start)
            .min(u64::from(geometry.inodes_per_group())) as usize;
        let bitmap = if uninitialised {
            let mut bitmap = vec![0; geometry.block_size() as usize];
            for bit in valid..bitmap.len() * 8 {
                bitmap[bit / 8] |= 1 << (bit % 8);
            }
            bitmap
        } else {
            self.view().read_bitmap(
                group.inode_bitmap,
                geometry.inodes_per_group(),
                group.inode_bitmap_checksum,
            )?
        };
        if number == 0 {
            let reserved = u32_at(&geometry.raw, 0x54)?
                .checked_sub(1)
                .ok_or(Error::Corrupt)? as usize;
            if reserved > valid || (0..reserved).any(|bit| bitmap[bit / 8] & (1 << (bit % 8)) == 0)
            {
                return Err(Error::Corrupt);
            }
        }
        let free = bitmap[..geometry.inodes_per_group() as usize / 8]
            .iter()
            .map(|byte| (!byte).count_ones())
            .sum::<u32>();
        if free != group.free_inodes {
            return Err(Error::Corrupt);
        }
        Ok(bitmap)
    }

    fn new_inode(
        &self,
        number: u32,
        kind: NewInodeKind<'_>,
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        let mut inode = Inode {
            number,
            raw: smallvec::smallvec![0; self.geometry.inode_size() as usize],
        };
        let mode = metadata.permissions
            | match kind {
                NewInodeKind::File => 0x8000,
                NewInodeKind::Directory => 0x4000,
                NewInodeKind::Symlink(_) => 0xa000,
                NewInodeKind::Special(kind) => kind.mode(),
            };
        inode.raw[..2].copy_from_slice(&mode.to_le_bytes());
        inode.raw[2..4].copy_from_slice(&(metadata.uid as u16).to_le_bytes());
        inode.raw[120..122].copy_from_slice(&((metadata.uid >> 16) as u16).to_le_bytes());
        inode.raw[24..26].copy_from_slice(&(metadata.gid as u16).to_le_bytes());
        inode.raw[122..124].copy_from_slice(&((metadata.gid >> 16) as u16).to_le_bytes());
        let links: u16 = match kind {
            NewInodeKind::File | NewInodeKind::Symlink(_) | NewInodeKind::Special(_) => 1,
            NewInodeKind::Directory => 2,
        };
        inode.raw[26..28].copy_from_slice(&links.to_le_bytes());
        if !matches!(kind, NewInodeKind::Special(_)) {
            inode.raw[32..36].copy_from_slice(&0x0008_0000u32.to_le_bytes());
        }
        inode.raw[100..104].copy_from_slice(&metadata.generation.to_le_bytes());
        if inode.raw.len() > 128 {
            inode.raw[128..130].copy_from_slice(&32u16.to_le_bytes());
        }
        metadata.created.store(&mut inode, 8, 140)?;
        metadata.created.store(&mut inode, 12, 132)?;
        metadata.created.store(&mut inode, 16, 136)?;
        if inode.raw.len() > 128 {
            metadata.created.store(&mut inode, 144, 148)?;
        }
        if !matches!(kind, NewInodeKind::Special(_)) {
            let root = Node::new(0, 4, Vec::new()).serialize(&inode, &self.geometry, 60)?;
            inode.raw[40..100].copy_from_slice(&root);
        }
        Ok(inode)
    }

    pub(super) fn allocate_block(&mut self, preferred: Option<u64>) -> Result<u64> {
        if self.geometry.free_blocks() == 0 {
            return Err(Error::NoSpace);
        }
        let goal = preferred
            .unwrap_or(self.allocation_cursor)
            .max(self.geometry.group_start(0));
        let goal = if goal < self.geometry.blocks() {
            goal
        } else {
            self.geometry.group_start(0)
        };
        let first_group = ((goal - self.geometry.group_start(0))
            / u64::from(self.geometry.blocks_per_group())) as u32;
        for step in 0..self.geometry.groups() {
            let number = ((u64::from(first_group) + u64::from(step))
                % u64::from(self.geometry.groups())) as u32;
            let group = self.view().group(number)?;
            if group.free_blocks == 0 {
                continue;
            }
            let mut bitmap = self.block_bitmap(number, &group)?;
            let start = self.geometry.group_start(number);
            let valid = (self.geometry.blocks() - start)
                .min(u64::from(self.geometry.blocks_per_group())) as usize;
            let first = if step == 0 {
                (goal - start) as usize
            } else {
                0
            };
            let index = (first..valid)
                .chain(0..first)
                .find(|index| bitmap[*index / 8] & (1 << (*index % 8)) == 0)
                .ok_or(Error::Corrupt)?;
            let block = start + index as u64;
            if self.reserved.contains(block) || block >= 1 << 48 {
                return Err(Error::Corrupt);
            }
            bitmap[index / 8] |= 1 << (index % 8);
            self.store_block_bitmap(number, group, bitmap, -1)?;
            self.allocation_cursor = block + 1;
            return Ok(block);
        }
        Err(Error::Corrupt)
    }

    pub(super) fn free_blocks(&mut self, start: u64, count: u32) -> Result<()> {
        if count == 0 || start < self.geometry.group_start(0) {
            return Err(Error::Corrupt);
        }
        let end = start.checked_add(u64::from(count)).ok_or(Error::Corrupt)?;
        let number = ((start - self.geometry.group_start(0))
            / u64::from(self.geometry.blocks_per_group())) as u32;
        let group_start = self.geometry.group_start(number);
        if end
            > (group_start + u64::from(self.geometry.blocks_per_group()))
                .min(self.geometry.blocks())
        {
            return Err(Error::Corrupt);
        }
        let group = self.view().group(number)?;
        let mut bitmap = self.block_bitmap(number, &group)?;
        for block in start..end {
            let bit = (block - group_start) as usize;
            let mask = 1 << (bit % 8);
            if self.reserved.contains(block) || bitmap[bit / 8] & mask == 0 {
                return Err(Error::Corrupt);
            }
            bitmap[bit / 8] &= !mask;
            self.metadata.remove(&block);
        }
        self.freed_blocks = true;
        self.store_block_bitmap(number, group, bitmap, count as i32)
    }

    fn block_bitmap(&self, number: u32, group: &Group) -> Result<Vec<u8>> {
        let geometry = &self.geometry;
        let start = geometry.group_start(number);
        let end = (start + u64::from(geometry.blocks_per_group())).min(geometry.blocks());
        let uninitialised = group.flags & BLOCK_UNINITIALISED != 0;
        let mut bitmap = if uninitialised {
            if !geometry.has_metadata_checksums() {
                return Err(Error::Unsupported);
            }
            let mut bitmap = vec![0; geometry.block_size() as usize];
            for bit in (end - start) as usize..bitmap.len() * 8 {
                bitmap[bit / 8] |= 1 << (bit % 8);
            }
            bitmap
        } else {
            self.view().read_bitmap(
                group.block_bitmap,
                geometry.blocks_per_group(),
                group.block_bitmap_checksum,
            )?
        };
        for range in self.reserved.within(start..end) {
            for block in range {
                let index = (block - start) as usize;
                let mask = 1 << (index % 8);
                if !uninitialised && bitmap[index / 8] & mask == 0 {
                    return Err(Error::Corrupt);
                }
                bitmap[index / 8] |= mask;
            }
        }
        let free = bitmap[..geometry.blocks_per_group() as usize / 8]
            .iter()
            .map(|byte| (!byte).count_ones())
            .sum::<u32>();
        if free != group.free_blocks {
            return Err(Error::Corrupt);
        }
        Ok(bitmap)
    }

    fn store_block_bitmap(
        &mut self,
        number: u32,
        mut group: Group,
        bitmap: Vec<u8>,
        delta: i32,
    ) -> Result<()> {
        let count = group
            .free_blocks
            .checked_add_signed(delta)
            .ok_or(Error::Corrupt)?;
        group.raw[12..14].copy_from_slice(&(count as u16).to_le_bytes());
        if group.raw.len() >= 64 {
            group.raw[44..46].copy_from_slice(&((count >> 16) as u16).to_le_bytes());
        }
        let flags = group.flags & !BLOCK_UNINITIALISED;
        group.raw[18..20].copy_from_slice(&flags.to_le_bytes());
        if self.geometry.has_metadata_checksums() {
            let crc = checksum::crc32c(
                self.geometry.checksum_seed(),
                &bitmap[..self.geometry.blocks_per_group() as usize / 8],
            );
            group.raw[24..26].copy_from_slice(&(crc as u16).to_le_bytes());
            if group.raw.len() >= 64 {
                group.raw[56..58].copy_from_slice(&((crc >> 16) as u16).to_le_bytes());
            }
        }
        self.metadata_block(group.block_bitmap)?
            .copy_from_slice(&bitmap);
        self.store_group(number, group)?;
        let count = self
            .geometry
            .free_blocks()
            .checked_add_signed(i64::from(delta))
            .ok_or(Error::Corrupt)?;
        let mut raw = self.geometry.raw;
        self.geometry
            .write_free_counts(&mut raw, count, self.geometry.free_inodes())?;
        self.store_superblock(raw)
    }

    fn store_group(&mut self, number: u32, mut group: Group) -> Result<()> {
        if self.geometry.has_metadata_checksums() {
            group.raw[30..32].fill(0);
            let crc = checksum::crc32c(self.geometry.checksum_seed(), &number.to_le_bytes());
            let crc = checksum::crc32c(crc, &group.raw);
            group.raw[30..32].copy_from_slice(&(crc as u16).to_le_bytes());
        }
        let offset = self.geometry.descriptor_offset(number)?;
        let block = offset / u64::from(self.geometry.block_size());
        let within = (offset % u64::from(self.geometry.block_size())) as usize;
        self.metadata_block(block)?[within..within + group.raw.len()].copy_from_slice(&group.raw);
        Ok(())
    }

    pub(super) fn store_superblock(&mut self, mut raw: [u8; 1024]) -> Result<()> {
        self.geometry.set_super_checksum(&mut raw);
        let geometry = Geometry::parse(raw, self.fs.device.len())?;
        let block = 1024 / u64::from(geometry.block_size());
        let within = 1024 % geometry.block_size() as usize;
        self.metadata_block(block)?[within..within + 1024].copy_from_slice(&raw);
        self.geometry = geometry;
        Ok(())
    }
}

impl<D: ReadAt> Ext4<D> {
    pub(crate) fn check_allocated_block(&self, physical: u64) -> Result<()> {
        self.geometry.block_offset(physical)?;
        let number = ((physical - self.geometry.group_start(0))
            / u64::from(self.geometry.blocks_per_group())) as u32;
        let group = self.group(number)?;
        if group.flags & BLOCK_UNINITIALISED != 0 {
            return Err(Error::Corrupt);
        }
        let bitmap = self.read_bitmap(
            group.block_bitmap(),
            self.geometry.blocks_per_group(),
            group.block_bitmap_checksum(),
        )?;
        let bit = (physical - self.geometry.group_start(number)) as usize;
        if bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
            return Err(Error::Corrupt);
        }
        Ok(())
    }
}

fn adjust_directories(group: &mut Group, delta: i32, maximum: u32) -> Result<()> {
    let count = u32::from(u16_at(&group.raw, 16)?)
        | if group.raw.len() >= 64 {
            u32::from(u16_at(&group.raw, 48)?) << 16
        } else {
            0
        };
    let count = count
        .checked_add_signed(delta)
        .filter(|count| *count <= maximum)
        .ok_or(Error::Corrupt)?;
    group.raw[16..18].copy_from_slice(&(count as u16).to_le_bytes());
    if group.raw.len() >= 64 {
        group.raw[48..50].copy_from_slice(&((count >> 16) as u16).to_le_bytes());
    }
    Ok(())
}
