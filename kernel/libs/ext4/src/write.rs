// SPDX-License-Identifier: MPL-2.0

//! Owned metadata updates and ordered file data writes.

use alloc::{collections::BTreeMap, vec, vec::Vec};

use io_util::{IoError, ReadAt, WriteAt, checksum};

use super::{
    Error, Ext4, Geometry, Inode, Journal, Result, SpecialFile, recovery::JournalMap, u16_at,
    u32_at,
};

mod allocation;
mod attribute;

pub use attribute::AttributeMode;
mod directory;
mod extent;
mod index;
mod link;
mod metadata;

pub use metadata::MetadataUpdate;
mod rename;
mod truncate;
mod unlink;

use allocation::Reserved;

/// Metadata supplied by the VFS when it creates an inode.
#[derive(Clone, Copy, Debug)]
pub struct CreateMetadata {
    pub permissions: u16,
    pub uid: u32,
    pub gid: u32,
    pub created: Timestamp,
    pub generation: u32,
}

#[derive(Clone, Copy)]
enum NewInodeKind<'a> {
    File,
    Directory,
    Symlink(&'a [u8]),
    Special(SpecialFile),
}

/// An ext4 timestamp within the extended on-disk time range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    /// Checks the seconds and nanoseconds before constructing a timestamp.
    pub fn new(seconds: i64, nanoseconds: u32) -> Result<Self> {
        if !(-2_147_483_648..=15_032_385_535).contains(&seconds) || nanoseconds >= 1_000_000_000 {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            seconds,
            nanoseconds,
        })
    }
    pub fn seconds(&self) -> i64 {
        self.seconds
    }
    pub fn nanoseconds(&self) -> u32 {
        self.nanoseconds
    }

    fn store(self, inode: &mut Inode, seconds_offset: usize, extra_offset: usize) -> Result<()> {
        let low = self.seconds as i32;
        let epoch = ((self.seconds - i64::from(low)) >> 32) as u32;
        let extra_size = if inode.raw.len() > 128 {
            usize::from(u16_at(&inode.raw, 128)?)
        } else {
            0
        };
        if extra_size >= extra_offset + 4 - 128 {
            inode.raw[extra_offset..extra_offset + 4]
                .copy_from_slice(&((self.nanoseconds << 2) | epoch).to_le_bytes());
        } else if epoch != 0 {
            return Err(Error::Unsupported);
        }
        inode.raw[seconds_offset..seconds_offset + 4].copy_from_slice(&low.to_le_bytes());
        Ok(())
    }
}

/// Bounds for recovery, mount admission and each owned transaction.
#[derive(Clone, Copy, Debug)]
pub struct MountLimits {
    /// Maximum journal length, in filesystem blocks.
    pub journal_blocks: u32,
    /// Maximum committed transactions retained during replay.
    pub transactions: usize,
    /// Maximum block revocations, including duplicates.
    pub revocations: usize,
    /// Maximum distinct physical runs and mapping blocks in the journal.
    pub mapping_entries: usize,
    /// Maximum orphan inodes retained during cleanup.
    pub orphan_inodes: usize,
    /// Maximum owned metadata blocks per transaction.
    pub metadata_blocks: usize,
    /// Maximum owned file-data blocks per transaction.
    pub data_blocks: usize,
    /// Maximum allocation groups checked when mounting.
    pub allocation_groups: u32,
    /// Maximum ancestors checked when moving a directory.
    pub directory_depth: usize,
}

enum Health {
    Ready,
    Failed,
}

struct JournalState {
    journal: Journal,
    map: JournalMap,
}

/// An exclusively owned write session with explicit sync and clean close.
///
/// Dropping this value leaves the on-disk recovery marker intact. An I/O
/// failure seals the session; subsequent operations return [`Error::Aborted`].
pub struct WritableExt4<D> {
    fs: Ext4<D>,
    journal: Option<JournalState>,
    limits: MountLimits,
    health: Health,
    reserved: Reserved,
    allocation_cursor: u64,
    pending_metadata: BTreeMap<u64, Vec<u8>>,
    pending_frees: bool,
}

impl<D: WriteAt> WritableExt4<D> {
    /// Recovers a volume and durably marks the new session as mounted.
    pub fn open(device: D, limits: MountLimits) -> Result<Self> {
        if device.is_read_only() {
            return Err(Error::ReadOnly);
        }
        if limits.metadata_blocks == 0 || limits.data_blocks == 0 {
            return Err(Error::ResourceLimit);
        }
        let fs = Ext4::recover(device, limits)?;
        fs.check_empty_orphans()?;
        Self::resume(fs, limits)
    }

    pub(super) fn resume(mut fs: Ext4<D>, limits: MountLimits) -> Result<Self> {
        if limits.metadata_blocks == 0 || limits.data_blocks == 0 {
            return Err(Error::ResourceLimit);
        }
        let journal = if let Some(journal) = fs.journal()? {
            if journal.blocks() > limits.journal_blocks {
                return Err(Error::ResourceLimit);
            }
            let map = fs.journal_map(&journal, limits.mapping_entries)?;
            Some(JournalState { journal, map })
        } else {
            None
        };
        let (reserved, free_blocks, free_inodes) =
            Reserved::scan(&fs, journal.as_ref().map(|state| &state.map), limits)?;
        let allocation_cursor = fs.geometry.group_start(0);
        let raw = fs.geometry.mounted_superblock(free_blocks, free_inodes)?;
        fs.device.write_all_at(1024, &raw)?;
        fs.device.flush()?;
        fs.geometry = Geometry::parse(raw, fs.device.len())?;
        Ok(Self {
            fs,
            journal,
            limits,
            health: Health::Ready,
            reserved,
            allocation_cursor,
            pending_metadata: BTreeMap::new(),
            pending_frees: false,
        })
    }

    pub fn geometry(&self) -> &Geometry {
        &self.fs.geometry
    }

    /// Returns read access while the write session is healthy.
    pub fn reader(&self) -> Result<&Ext4<D>> {
        self.check_health()?;
        Ok(&self.fs)
    }

    /// Writes a bounded batch, allocating extents and zeroing newly exposed bytes.
    ///
    /// A larger request can return a short write at the configured transaction boundary.
    pub fn write_at(
        &mut self,
        number: u32,
        offset: u64,
        data: &[u8],
        modified: Timestamp,
    ) -> Result<usize> {
        self.update(|transaction| {
            let mut inode = transaction.mutable_inode(number)?;
            if inode.mode() & 0xf000 != 0x8000 {
                return Err(Error::InvalidInput);
            }
            let flags = u32_at(&inode.raw, 32)?;
            if flags & 0x10 != 0 || (flags & 0x20 != 0 && offset != inode.size()) {
                return Err(Error::PermissionDenied);
            }
            if data.is_empty() {
                return Ok(0);
            }
            let block_size = u64::from(transaction.fs.geometry.block_size());
            let old_size = inode.size();
            let gap_block = if offset > old_size {
                transaction
                    .zero_tail(&inode, old_size)?
                    .filter(|_| offset / block_size != old_size / block_size)
            } else {
                None
            };
            let available = transaction.limits.data_blocks - usize::from(gap_block.is_some());
            if available == 0 {
                return Err(Error::ResourceLimit);
            }
            let capacity = available
                .checked_mul(block_size as usize)
                .ok_or(Error::ResourceLimit)?;
            let count = data.len().min(capacity - (offset % block_size) as usize);
            let end = offset
                .checked_add(count as u64)
                .ok_or(Error::InvalidInput)?;
            if (end - 1) / block_size > u64::from(u32::MAX) {
                return Err(Error::InvalidInput);
            }
            let mut copied = 0;
            while copied < count {
                let position = offset + copied as u64;
                let logical =
                    u32::try_from(position / block_size).map_err(|_| Error::InvalidInput)?;
                let (physical, new) = transaction.ensure_file_block(&mut inode, logical)?;
                transaction.check_file_block(physical)?;
                let within = (position % block_size) as usize;
                let length = (block_size as usize - within).min(count - copied);
                let mut raw = if let Some(raw) = transaction.data.get(&physical) {
                    raw.clone()
                } else if new || (within == 0 && length == block_size as usize) {
                    vec![0; block_size as usize]
                } else {
                    transaction.fs.read_data_block(physical)?
                };
                raw[within..within + length].copy_from_slice(&data[copied..copied + length]);
                transaction.data.insert(physical, raw);
                copied += length;
            }
            let size = old_size.max(end);
            inode.set_size(size);
            modified.store(&mut inode, 16, 136)?;
            modified.store(&mut inode, 12, 132)?;
            transaction.store_inode(inode)?;
            Ok(count)
        })
    }

    /// Flushes the device and reports failure without reopening a sealed session.
    pub fn sync(&mut self) -> Result<()> {
        self.check_health()?;
        if let Err(error) = self
            .commit_pending()
            .and_then(|()| self.fs.device.flush().map_err(Into::into))
        {
            self.health = Health::Failed;
            return Err(error);
        }
        Ok(())
    }

    /// Flushes all updates and durably closes the volume as clean.
    pub fn close(mut self) -> Result<Ext4<D>> {
        self.sync()?;
        self.fs.check_empty_orphans()?;
        let raw = self.fs.geometry.clean_superblock();
        self.fs.device.write_all_at(1024, &raw)?;
        self.fs.device.flush()?;
        self.fs.geometry = Geometry::parse(raw, self.fs.device.len())?;
        Ok(self.fs)
    }

    fn run_cleanup(&mut self, cleanup: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if let Err(error) = cleanup(self) {
            self.health = Health::Failed;
            return Err(error);
        }
        Ok(())
    }

    fn check_health(&self) -> Result<()> {
        match self.health {
            Health::Ready => Ok(()),
            Health::Failed => Err(Error::Aborted),
        }
    }

    fn update<T>(
        &mut self,
        update_fn: impl FnOnce(&mut Transaction<'_, D>) -> Result<T>,
    ) -> Result<T> {
        self.check_health()?;
        let mut transaction = Transaction {
            fs: &self.fs,
            geometry: self.fs.geometry.clone(),
            reserved: &self.reserved,
            allocation_cursor: self.allocation_cursor,
            limits: self.limits,
            metadata: BTreeMap::new(),
            data: BTreeMap::new(),
            freed_blocks: false,
        };
        let result = update_fn(&mut transaction)?;
        if let Some(state) = &self.journal {
            for (block, raw) in &transaction.metadata {
                state
                    .map
                    .check_target(&self.fs, &state.journal, *block, raw)?;
            }
            for (block, raw) in &transaction.data {
                state
                    .map
                    .check_target(&self.fs, &state.journal, *block, raw)?;
            }
        }
        let freed_blocks = transaction.freed_blocks;
        let geometry = transaction.geometry;
        let allocation_cursor = transaction.allocation_cursor;
        let metadata = transaction.metadata;
        let data = transaction.data;
        let persist_fn = || -> Result<()> {
            let limit = (self.fs.device.metadata_writeback_limit()
                / self.fs.geometry.block_size() as usize)
                .min(self.limits.metadata_blocks);
            // Commit frees before data can overwrite blocks still owned on disk.
            if self.pending_frees && !data.is_empty() {
                self.commit_pending()?;
            }
            self.write_blocks(&data)?;
            if !data.is_empty() && limit == 0 {
                self.fs.device.flush()?;
            }
            if limit != 0 {
                let added = metadata
                    .keys()
                    .filter(|block| !self.pending_metadata.contains_key(block))
                    .count();
                if self.pending_metadata.len() + added > limit {
                    self.commit_pending()?;
                }
                for (block, raw) in metadata {
                    self.fs
                        .device
                        .stage_metadata(self.fs.geometry.block_offset(block)?, &raw)?;
                    self.pending_metadata.insert(block, raw);
                }
                self.pending_frees |= freed_blocks;
            } else {
                self.commit_metadata(&metadata)?;
            }
            Ok(())
        };
        if let Err(error) = persist_fn() {
            self.health = Health::Failed;
            return Err(error);
        }
        self.fs.geometry = geometry;
        self.allocation_cursor = allocation_cursor;
        Ok(result)
    }

    fn commit_metadata(&mut self, metadata: &BTreeMap<u64, Vec<u8>>) -> Result<()> {
        if let Some(state) = &mut self.journal {
            state
                .journal
                .commit(&self.fs.device, &self.fs.geometry, &state.map, metadata)?;
        } else {
            self.write_blocks(metadata)?;
            if !metadata.is_empty() {
                self.fs.device.flush()?;
            }
        }
        Ok(())
    }

    fn write_blocks(&self, blocks: &BTreeMap<u64, Vec<u8>>) -> Result<()> {
        let mut entries = blocks.iter().peekable();
        while let Some((&block, raw)) = entries.next() {
            let mut last = block;
            let mut bytes: Option<Vec<u8>> = None;
            while let Some((&next, next_raw)) = entries.peek().copied() {
                let length = bytes.as_ref().map_or(raw.len(), Vec::len);
                if last.checked_add(1) != Some(next) || length + next_raw.len() > 65536 {
                    break;
                }
                bytes
                    .get_or_insert_with(|| raw.clone())
                    .extend_from_slice(next_raw);
                last = next;
                entries.next();
            }
            self.fs.device.write_all_at(
                self.fs.geometry.block_offset(block)?,
                bytes.as_deref().unwrap_or(raw),
            )?;
        }
        Ok(())
    }

    fn commit_pending(&mut self) -> Result<()> {
        if self.pending_metadata.is_empty() {
            return Ok(());
        }
        self.fs.device.flush()?;
        let metadata = core::mem::take(&mut self.pending_metadata);
        self.commit_metadata(&metadata)?;
        self.fs.device.clear_staged_metadata();
        self.pending_frees = false;
        Ok(())
    }
}

struct Transaction<'a, D> {
    fs: &'a Ext4<D>,
    geometry: Geometry,
    reserved: &'a Reserved,
    allocation_cursor: u64,
    limits: MountLimits,
    metadata: BTreeMap<u64, Vec<u8>>,
    data: BTreeMap<u64, Vec<u8>>,
    freed_blocks: bool,
}

struct Overlay<'a, D> {
    device: &'a D,
    block_size: u32,
    metadata: &'a BTreeMap<u64, Vec<u8>>,
}

impl<D: ReadAt> ReadAt for Overlay<'_, D> {
    fn len(&self) -> u64 {
        self.device.len()
    }
    fn metadata_crc32c(&self, offset: u64, bytes: &[u8], seed: u32) -> u32 {
        self.device.metadata_crc32c(offset, bytes, seed)
    }
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> core::result::Result<(), IoError> {
        if offset
            .checked_add(output.len() as u64)
            .is_none_or(|end| end > self.len())
        {
            return Err(IoError::Failed);
        }
        let mut copied = 0;
        while copied < output.len() {
            let position = offset + copied as u64;
            let block = position / u64::from(self.block_size);
            let within = (position % u64::from(self.block_size)) as usize;
            let length = (self.block_size as usize - within).min(output.len() - copied);
            if let Some(raw) = self.metadata.get(&block) {
                output[copied..copied + length].copy_from_slice(&raw[within..within + length]);
            } else {
                self.device
                    .read_metadata_exact_at(position, &mut output[copied..copied + length])?;
            }
            copied += length;
        }
        Ok(())
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    fn check_file_block(&self, physical: u64) -> Result<()> {
        if self.reserved.contains(physical) {
            return Err(Error::Corrupt);
        }
        self.view().check_allocated_block(physical)
    }

    fn check_attribute_block(&self, inode: &Inode) -> Result<()> {
        let physical = inode.attribute_block_number()?;
        if physical != 0 {
            self.check_file_block(physical)?;
        }
        Ok(())
    }

    fn zero_tail(&mut self, inode: &Inode, position: u64) -> Result<Option<u64>> {
        let block_size = u64::from(self.geometry.block_size());
        if position.is_multiple_of(block_size) || inode.is_inline_symlink(&self.geometry)? {
            return Ok(None);
        }
        let logical = u32::try_from(position / block_size).map_err(|_| Error::InvalidInput)?;
        let Some(physical) = self.view().file_block(inode, logical)? else {
            return Ok(None);
        };
        self.check_file_block(physical)?;
        let mut raw = self.fs.read_data_block(physical)?;
        raw[(position % block_size) as usize..].fill(0);
        self.data.insert(physical, raw);
        Ok(Some(physical))
    }

    fn store_entry_change(
        &mut self,
        mut parent: Inode,
        mut inode: Inode,
        changed: Timestamp,
    ) -> Result<Inode> {
        changed.store(&mut parent, 16, 136)?;
        changed.store(&mut parent, 12, 132)?;
        changed.store(&mut inode, 12, 132)?;
        let number = inode.number();
        self.store_inode(parent)?;
        self.store_inode(inode)?;
        self.view().inode(number)
    }

    fn ensure_missing(&self, parent: &Inode, name: &[u8]) -> Result<()> {
        match self.view().lookup(parent, name) {
            Ok(_) => Err(Error::AlreadyExists),
            Err(Error::NotFound) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn directory_for_update(&self, number: u32) -> Result<Inode> {
        let inode = self.view().inode(number)?;
        if inode.mode() & 0xf000 != 0x4000 {
            return Err(Error::NotDirectory);
        }
        if inode.links() == 0 {
            return Err(Error::NotFound);
        }
        let flags = u32_at(&inode.raw, 32)?;
        if flags & 0x10 != 0 {
            return Err(Error::PermissionDenied);
        }
        Ok(inode)
    }

    fn metadata_inode(&self, number: u32) -> Result<Inode> {
        if number != 2 {
            return self.mutable_inode(number);
        }
        let inode = self.view().inode(number)?;
        if inode.mode() & 0xf000 != 0x4000 {
            return Err(Error::Corrupt);
        }
        Ok(inode)
    }

    fn mutable_inode(&self, number: u32) -> Result<Inode> {
        if number < u32_at(&self.geometry.raw, 0x54)?
            || number == self.geometry.journal_inode()
            || number == self.geometry.orphan_file_inode()
        {
            return Err(Error::PermissionDenied);
        }
        self.view().inode(number)
    }

    fn view(&self) -> Ext4<Overlay<'_, D>> {
        Ext4 {
            device: Overlay {
                device: &self.fs.device,
                block_size: self.fs.geometry.block_size(),
                metadata: &self.metadata,
            },
            geometry: self.geometry.clone(),
        }
    }

    fn store_inode(&mut self, mut inode: Inode) -> Result<()> {
        let geometry = &self.geometry;
        let group = self
            .view()
            .group((inode.number() - 1) / geometry.inodes_per_group())?;
        if geometry.has_metadata_checksums() {
            let extra = if inode.raw.len() > 128 {
                u16_at(&inode.raw, 128)?
            } else {
                0
            };
            inode.raw[124..126].fill(0);
            if extra >= 4 {
                inode.raw[130..132].fill(0);
            }
            let crc = checksum::crc32c(geometry.checksum_seed(), &inode.number().to_le_bytes());
            let crc = checksum::crc32c(crc, &u32_at(&inode.raw, 100)?.to_le_bytes());
            let crc = checksum::crc32c(crc, &inode.raw);
            inode.raw[124..126].copy_from_slice(&(crc as u16).to_le_bytes());
            if extra >= 4 {
                inode.raw[130..132].copy_from_slice(&((crc >> 16) as u16).to_le_bytes());
            }
        }
        let offset = geometry.block_offset(group.inode_table())?
            + u64::from((inode.number() - 1) % geometry.inodes_per_group())
                * u64::from(geometry.inode_size());
        let block = offset / u64::from(geometry.block_size());
        let within = (offset % u64::from(geometry.block_size())) as usize;
        let mut previous = vec![0; inode.raw.len()];
        self.view().device.read_exact_at(offset, &mut previous)?;
        if previous == inode.raw {
            return Ok(());
        }
        let raw = self.metadata_block(block)?;
        raw[within..within + inode.raw.len()].copy_from_slice(&inode.raw);
        Ok(())
    }

    fn metadata_block(&mut self, block: u64) -> Result<&mut Vec<u8>> {
        if !self.metadata.contains_key(&block) {
            if self.metadata.len() >= self.limits.metadata_blocks {
                return Err(Error::ResourceLimit);
            }
            self.metadata.insert(block, self.fs.read_block(block)?);
        }
        self.metadata.get_mut(&block).ok_or(Error::Corrupt)
    }
}

impl Inode {
    fn set_size(&mut self, size: u64) {
        self.raw[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        self.raw[108..112].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
    }
}
