// SPDX-License-Identifier: MPL-2.0

//! Atomic inode and directory-entry creation.

use alloc::{vec, vec::Vec};

use io_util::{ReadAt, WriteAt, checksum};

use super::{CreateMetadata, NewInodeKind, Transaction, WritableExt4};
use crate::{
    Error, Inode, Result, SpecialFile, directory, directory::index::INDEX_FLAG, u16_at, u32_at,
};

pub(super) struct LeafEntry {
    pub inode: u32,
    pub name: Vec<u8>,
    pub file_type: u8,
}

impl LeafEntry {
    pub(super) fn encoded_size(&self) -> usize {
        (8 + self.name.len()).next_multiple_of(4)
    }
}

impl From<crate::DirEntry> for LeafEntry {
    fn from(entry: crate::DirEntry) -> Self {
        let inode = entry.inode();
        let file_type = entry.file_type();
        Self {
            inode,
            name: entry.into_name(),
            file_type,
        }
    }
}

impl<D: WriteAt> WritableExt4<D> {
    /// Creates a regular file in a directory.
    pub fn create_file(
        &mut self,
        parent: u32,
        name: &[u8],
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        self.create_child(parent, name, NewInodeKind::File, metadata)
    }

    /// Creates a directory, including its parent link, in a directory.
    pub fn create_directory(
        &mut self,
        parent: u32,
        name: &[u8],
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        self.create_child(parent, name, NewInodeKind::Directory, metadata)
    }

    /// Creates an inline or extent-backed symbolic link in a directory.
    pub fn create_symlink(
        &mut self,
        parent: u32,
        name: &[u8],
        target: &[u8],
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        if target.is_empty() || target.contains(&0) {
            return Err(Error::InvalidInput);
        }
        if target.len() >= self.geometry().block_size() as usize {
            return Err(Error::NameTooLong);
        }
        self.create_child(parent, name, NewInodeKind::Symlink(target), metadata)
    }

    /// Creates a FIFO, socket inode or device node in a directory.
    pub fn create_special(
        &mut self,
        parent: u32,
        name: &[u8],
        kind: SpecialFile,
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        self.create_child(parent, name, NewInodeKind::Special(kind), metadata)
    }

    /// Allocates an orphaned regular inode for a temporary open file.
    pub fn create_tmpfile(&mut self, parent: u32, metadata: CreateMetadata) -> Result<Inode> {
        self.update(|transaction| {
            let parent = transaction.metadata_inode(parent)?;
            if parent.mode() & 0xf000 != 0x4000 {
                return Err(Error::NotDirectory);
            }
            if parent.links() == 0 {
                return Err(Error::NotFound);
            }
            if parent.flags() & 0x10 != 0 {
                return Err(Error::PermissionDenied);
            }
            let mut inode =
                transaction.allocate_inode(parent.number(), NewInodeKind::File, metadata)?;
            inode.raw[26..28].fill(0);
            transaction.add_orphan(&mut inode)?;
            let number = inode.number();
            transaction.store_inode(inode)?;
            transaction.view().inode(number)
        })
    }

    fn create_child(
        &mut self,
        parent: u32,
        name: &[u8],
        kind: NewInodeKind<'_>,
        metadata: CreateMetadata,
    ) -> Result<Inode> {
        directory::check_child_name(name)?;
        self.update(|transaction| {
            let mut parent = transaction.directory_for_update(parent)?;
            transaction.ensure_missing(&parent, name)?;
            let mut inode = transaction.allocate_inode(parent.number(), kind, metadata)?;
            let file_type = match kind {
                NewInodeKind::File => 1,
                NewInodeKind::Special(kind) => {
                    if let SpecialFile::Character(number) | SpecialFile::Block(number) = kind {
                        inode.raw[44..48].copy_from_slice(&number.encoded().to_le_bytes());
                    }
                    inode.directory_type()?
                }
                NewInodeKind::Symlink(target) => {
                    transaction.initialise_symlink(&mut inode, target)?;
                    7
                }
                NewInodeKind::Directory => {
                    transaction.initialise_directory(&mut inode, parent.number())?;
                    transaction.change_directory_links(&mut parent, 1)?;
                    2
                }
            };
            transaction.insert_entry(&mut parent, name, inode.number(), file_type)?;
            transaction.store_entry_change(parent, inode, metadata.created)
        })
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    pub(super) fn change_directory_links(&self, inode: &mut Inode, delta: i16) -> Result<()> {
        let large = self.geometry.has_large_directory_link_counts();
        if large && inode.links() == 1 {
            return Ok(());
        }
        let links = inode
            .links()
            .checked_add_signed(delta)
            .filter(|count| *count >= 2)
            .ok_or(Error::Corrupt)?;
        let links = if links >= 65000 {
            if !large {
                return Err(Error::TooManyLinks);
            }
            1
        } else {
            links
        };
        inode.raw[26..28].copy_from_slice(&links.to_le_bytes());
        Ok(())
    }

    fn initialise_symlink(&mut self, inode: &mut Inode, target: &[u8]) -> Result<()> {
        if target.len() < 60 {
            inode.raw[32..36].fill(0);
            inode.raw[40..100].fill(0);
            inode.raw[40..40 + target.len()].copy_from_slice(target);
        } else {
            let (physical, _) = self.ensure_file_block(inode, 0)?;
            let mut raw = vec![0; self.geometry.block_size() as usize];
            raw[..target.len()].copy_from_slice(target);
            self.data.insert(physical, raw);
        }
        inode.set_size(target.len() as u64);
        Ok(())
    }

    fn initialise_directory(&mut self, inode: &mut Inode, parent: u32) -> Result<()> {
        let block_size = self.geometry.block_size();
        let (physical, _) = self.ensure_file_block(inode, 0)?;
        let mut raw = vec![0; block_size as usize];
        let usable = self.directory_usable();
        put_entry(
            &mut raw,
            0,
            12,
            inode.number(),
            b".",
            2,
            self.geometry.has_directory_types(),
        )?;
        put_entry(
            &mut raw,
            12,
            usable - 12,
            parent,
            b"..",
            2,
            self.geometry.has_directory_types(),
        )?;
        self.set_directory_checksum(inode, &mut raw)?;
        self.metadata_block(physical)?.copy_from_slice(&raw);
        inode.raw[4..8].copy_from_slice(&block_size.to_le_bytes());
        Ok(())
    }

    pub(super) fn insert_entry(
        &mut self,
        inode: &mut Inode,
        name: &[u8],
        number: u32,
        file_type: u8,
    ) -> Result<()> {
        let blocks = u32::try_from(inode.size() / u64::from(self.geometry.block_size()))
            .map_err(|_| Error::Corrupt)?;
        if inode.flags() & INDEX_FLAG != 0 {
            return self.insert_indexed(inode, name, number, file_type);
        }
        for logical in 0..blocks {
            if self.insert_in_leaf(inode, logical, name, number, file_type)? {
                return Ok(());
            }
        }
        if blocks == 1 && u32_at(&self.geometry.raw, 0x5c)? & 0x20 != 0 {
            self.index_directory(inode)?;
            return self.insert_indexed(inode, name, number, file_type);
        }
        let has_types = self.geometry.has_directory_types();
        let usable = self.directory_usable();
        let (physical, _) = self.ensure_file_block(inode, blocks)?;
        let mut raw = vec![0; self.geometry.block_size() as usize];
        put_entry(&mut raw, 0, usable, number, name, file_type, has_types)?;
        self.set_directory_checksum(inode, &mut raw)?;
        self.metadata_block(physical)?.copy_from_slice(&raw);
        let size = inode
            .size()
            .checked_add(u64::from(self.geometry.block_size()))
            .ok_or(Error::InvalidInput)?;
        inode.raw[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        inode.raw[108..112].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        Ok(())
    }

    pub(super) fn insert_in_leaf(
        &mut self,
        inode: &Inode,
        logical: u32,
        name: &[u8],
        number: u32,
        file_type: u8,
    ) -> Result<bool> {
        let needed = (8 + name.len()).next_multiple_of(4);
        let has_types = self.geometry.has_directory_types();
        let usable = self.directory_usable();
        let physical = self
            .view()
            .file_block(inode, logical)?
            .ok_or(Error::Corrupt)?;
        self.check_file_block(physical)?;
        let mut raw = self.view().directory_leaf(inode, logical)?;
        let mut within = 0;
        while within < usable {
            let occupied = u32_at(&raw, within)? != 0;
            let length = usize::from(u16_at(&raw, within + 4)?);
            let name_length = if has_types {
                usize::from(raw[within + 6])
            } else {
                usize::from(u16_at(&raw, within + 6)?)
            };
            let used = if occupied {
                (8 + name_length).next_multiple_of(4)
            } else {
                0
            };
            if length < 8 || !length.is_multiple_of(4) || length > usable - within || used > length
            {
                return Err(Error::Corrupt);
            }
            if length - used >= needed {
                if occupied {
                    raw[within + 4..within + 6].copy_from_slice(&(used as u16).to_le_bytes());
                }
                put_entry(
                    &mut raw,
                    within + used,
                    length - used,
                    number,
                    name,
                    file_type,
                    has_types,
                )?;
                self.set_directory_checksum(inode, &mut raw)?;
                self.metadata_block(physical)?.copy_from_slice(&raw);
                return Ok(true);
            }
            within += length;
        }
        Ok(false)
    }

    pub(super) fn encode_leaf(&self, inode: &Inode, entries: &[LeafEntry]) -> Result<Vec<u8>> {
        let mut raw = vec![0; self.geometry.block_size() as usize];
        let usable = self.directory_usable();
        let mut position = 0;
        for (index, entry) in entries.iter().enumerate() {
            let length = if index + 1 == entries.len() {
                usable.checked_sub(position).ok_or(Error::Corrupt)?
            } else {
                entry.encoded_size()
            };
            put_entry(
                &mut raw,
                position,
                length,
                entry.inode,
                &entry.name,
                entry.file_type,
                self.geometry.has_directory_types(),
            )?;
            position += length;
        }
        if entries.is_empty() {
            put_entry(
                &mut raw,
                0,
                usable,
                0,
                b"",
                0,
                self.geometry.has_directory_types(),
            )?;
        }
        self.set_directory_checksum(inode, &mut raw)?;
        Ok(raw)
    }

    fn directory_usable(&self) -> usize {
        self.geometry.block_size() as usize
            - if self.geometry.has_metadata_checksums() {
                12
            } else {
                0
            }
    }

    pub(super) fn set_directory_checksum(&self, inode: &Inode, raw: &mut [u8]) -> Result<()> {
        if !self.geometry.has_metadata_checksums() {
            return Ok(());
        }
        let tail = raw.len() - 12;
        raw[tail..tail + 8].copy_from_slice(&[0, 0, 0, 0, 12, 0, 0, 0xde]);
        let seed = checksum::crc32c(self.geometry.checksum_seed(), &inode.number().to_le_bytes());
        let seed = checksum::crc32c(seed, &u32_at(&inode.raw, 100)?.to_le_bytes());
        let crc = checksum::crc32c(seed, &raw[..tail]);
        raw[tail + 8..].copy_from_slice(&crc.to_le_bytes());
        Ok(())
    }
}

fn put_entry(
    raw: &mut [u8],
    offset: usize,
    length: usize,
    number: u32,
    name: &[u8],
    file_type: u8,
    has_types: bool,
) -> Result<()> {
    if name.len() > 255
        || length < 8 + name.len()
        || !length.is_multiple_of(4)
        || offset.checked_add(length).is_none_or(|end| end > raw.len())
    {
        return Err(Error::Corrupt);
    }
    let record = &mut raw[offset..offset + length];
    record.fill(0);
    record[..4].copy_from_slice(&number.to_le_bytes());
    record[4..6].copy_from_slice(&(length as u16).to_le_bytes());
    if has_types {
        record[6] = name.len() as u8;
        record[7] = file_type;
    } else {
        record[6..8].copy_from_slice(&(name.len() as u16).to_le_bytes());
    }
    record[8..8 + name.len()].copy_from_slice(name);
    Ok(())
}
