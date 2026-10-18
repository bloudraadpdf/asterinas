// SPDX-License-Identifier: MPL-2.0

//! Directory iteration with leaf and HTree metadata integrity checks.

use alloc::vec::Vec;

use io_util::{ReadAt, checksum};

use super::{Error, Ext4, Inode, Result, u16_at, u32_at};

pub(crate) mod hash;
pub(crate) mod index;

use index::{INDEX_FLAG, Node};

/// One live directory entry and the next iteration position.
#[derive(Clone, Debug)]
pub struct DirEntry {
    inode: u32,
    name: Vec<u8>,
    file_type: u8,
    next_offset: u64,
}

impl DirEntry {
    pub fn inode(&self) -> u32 {
        self.inode
    }
    pub fn name(&self) -> &[u8] {
        &self.name
    }
    /// Returns the ext4 directory type code, or zero when unspecified.
    pub fn file_type(&self) -> u8 {
        self.file_type
    }
    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }
    pub(crate) fn into_name(self) -> Vec<u8> {
        self.name
    }
}

/// A streaming directory cursor that retains one checked block.
pub struct Directory<'a, D> {
    fs: &'a Ext4<D>,
    inode: &'a Inode,
    position: u64,
    cached_block: Option<u32>,
    index_blocks: Option<Vec<u32>>,
    raw: Vec<u8>,
    usable: usize,
    failed: bool,
}

impl<D: ReadAt> Ext4<D> {
    /// Iterates a directory from its beginning.
    pub fn read_dir<'a>(&'a self, inode: &'a Inode) -> Result<Directory<'a, D>> {
        self.read_dir_from(inode, 0)
    }

    /// Iterates from a previously returned directory position.
    pub fn read_dir_from<'a>(&'a self, inode: &'a Inode, offset: u64) -> Result<Directory<'a, D>> {
        if inode.mode() & 0xf000 != 0x4000 {
            return Err(Error::NotDirectory);
        }
        if inode.size() == 0
            || !inode
                .size()
                .is_multiple_of(u64::from(self.geometry.block_size()))
        {
            return Err(Error::Corrupt);
        }
        if offset > inode.size() || !offset.is_multiple_of(4) {
            return Err(Error::InvalidInput);
        }
        Ok(Directory {
            fs: self,
            inode,
            position: offset,
            cached_block: None,
            index_blocks: None,
            raw: Vec::new(),
            usable: 0,
            failed: false,
        })
    }

    /// Resolves a byte name without assuming UTF-8 encoding.
    pub fn lookup(&self, directory: &Inode, name: &[u8]) -> Result<Inode> {
        check_name(name)?;
        self.inode(self.find_entry(directory, name)?.inode())
    }

    pub(crate) fn directory_leaf(&self, inode: &Inode, logical: u32) -> Result<Vec<u8>> {
        let mut cursor = self.read_dir(inode)?;
        cursor.load_leaf(logical)?;
        Ok(cursor.raw)
    }

    pub(crate) fn leaf_entries(&self, inode: &Inode, logical: u32) -> Result<Vec<DirEntry>> {
        let start = u64::from(logical) * u64::from(self.geometry.block_size());
        let mut cursor = self.read_dir_from(inode, start)?;
        cursor.load_leaf(logical)?;
        let mut entries = Vec::new();
        while let Some(entry) =
            cursor.advance_until(start + u64::from(self.geometry.block_size()))?
        {
            entries.push(entry);
        }
        Ok(entries)
    }

    pub(crate) fn find_entry(&self, directory: &Inode, name: &[u8]) -> Result<DirEntry> {
        let mut cursor = self.read_dir(directory)?;
        if directory.flags() & INDEX_FLAG == 0 || name == b"." || name == b".." {
            return cursor.find_name(name, directory.size());
        }
        let mut path = self.index_path(directory, name)?;
        let block_size = u64::from(self.geometry.block_size());
        loop {
            cursor.position = u64::from(path.leaf()) * block_size;
            cursor.load_leaf(path.leaf())?;
            match cursor.find_name(name, cursor.position + block_size) {
                Ok(entry) => return Ok(entry),
                Err(Error::NotFound) => (),
                Err(error) => return Err(error),
            }
            if !path.next(self, directory)? {
                return Err(Error::NotFound);
            }
        }
    }
}

impl<D: ReadAt> Iterator for Directory<'_, D> {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.advance_until(self.inode.size()) {
            Ok(entry) => entry.map(Ok),
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

impl<D: ReadAt> Directory<'_, D> {
    fn load_leaf(&mut self, logical: u32) -> Result<()> {
        self.load(logical)?;
        if self.inode.flags() & INDEX_FLAG != 0
            && (logical == 0
                || self
                    .index_blocks
                    .as_ref()
                    .is_some_and(|blocks| blocks.binary_search(&logical).is_ok()))
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn find_name(&mut self, name: &[u8], end: u64) -> Result<DirEntry> {
        while let Some(entry) = self.advance_until(end)? {
            if entry.name() == name {
                return Ok(entry);
            }
        }
        Err(Error::NotFound)
    }

    fn advance_until(&mut self, end: u64) -> Result<Option<DirEntry>> {
        let block_size = u64::from(self.fs.geometry.block_size());
        while self.position < end {
            let logical = u32::try_from(self.position / block_size).map_err(|_| Error::Corrupt)?;
            if self.cached_block != Some(logical) {
                self.load(logical)?;
            }
            let within = (self.position % block_size) as usize;
            if within >= self.usable {
                self.position = (u64::from(logical) + 1) * block_size;
                continue;
            }
            if within + 8 > self.usable {
                return Err(Error::Corrupt);
            }
            let number = u32_at(&self.raw, within)?;
            let length = usize::from(u16_at(&self.raw, within + 4)?);
            let name_length = if self.fs.geometry.has_directory_types() {
                usize::from(self.raw[within + 6])
            } else {
                usize::from(u16_at(&self.raw, within + 6)?)
            };
            if length < 8
                || !length.is_multiple_of(4)
                || length > self.usable - within
                || name_length > length - 8
                || name_length > 255
            {
                return Err(Error::Corrupt);
            }
            self.position += length as u64;
            if number == 0 {
                continue;
            }
            if number > self.fs.geometry.inodes() || name_length == 0 {
                return Err(Error::Corrupt);
            }
            let name = &self.raw[within + 8..within + 8 + name_length];
            if name.contains(&0) || name.contains(&b'/') {
                return Err(Error::Corrupt);
            }
            let file_type = if self.fs.geometry.has_directory_types() {
                self.raw[within + 7]
            } else {
                0
            };
            if file_type > 7 {
                return Err(Error::Corrupt);
            }
            return Ok(Some(DirEntry {
                inode: number,
                name: name.to_vec(),
                file_type,
                next_offset: self.position,
            }));
        }
        Ok(None)
    }

    fn load(&mut self, logical: u32) -> Result<()> {
        let block = self
            .fs
            .file_block(self.inode, logical)?
            .ok_or(Error::Corrupt)?;
        self.raw = self.fs.read_block(block)?;
        self.usable = self.raw.len();
        let indexed = u32_at(&self.inode.raw, 32)? & INDEX_FLAG != 0;
        if indexed && self.index_blocks.is_none() {
            let root = if logical == 0 {
                Node::parse(&self.fs.geometry, self.inode, 0, self.raw.clone())?
            } else {
                self.fs.index_node(self.inode, 0)?
            };
            let mut blocks = if root.raw[30] == 0 {
                Vec::new()
            } else {
                root.entries.iter().map(|entry| entry.block).collect()
            };
            blocks.sort_unstable();
            self.index_blocks = Some(blocks);
        }
        let index_node = self
            .index_blocks
            .as_ref()
            .is_some_and(|blocks| blocks.binary_search(&logical).is_ok());
        if indexed && (logical == 0 || index_node) {
            Node::parse(&self.fs.geometry, self.inode, logical, self.raw.clone())?;
        } else if self.fs.geometry.has_metadata_checksums() {
            let tail = self.raw.len() - 12;
            if u32_at(&self.raw, tail)? != 0
                || u16_at(&self.raw, tail + 4)? != 12
                || self.raw[tail + 6] != 0
                || self.raw[tail + 7] != 0xde
            {
                return Err(Error::Corrupt);
            }
            if checksum::crc32c(self.checksum_seed()?, &self.raw[..tail])
                != u32_at(&self.raw, tail + 8)?
            {
                return Err(Error::Corrupt);
            }
            self.usable = tail;
        }
        self.cached_block = Some(logical);
        Ok(())
    }

    fn checksum_seed(&self) -> Result<u32> {
        let seed = checksum::crc32c(
            self.fs.geometry.checksum_seed(),
            &self.inode.number().to_le_bytes(),
        );
        Ok(checksum::crc32c(
            seed,
            &u32_at(&self.inode.raw, 100)?.to_le_bytes(),
        ))
    }
}

pub(super) fn check_name(name: &[u8]) -> Result<()> {
    if name.len() > 255 {
        return Err(Error::NameTooLong);
    }
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') {
        return Err(Error::InvalidInput);
    }
    Ok(())
}

pub(super) fn check_child_name(name: &[u8]) -> Result<()> {
    check_name(name)?;
    if name == b"." || name == b".." {
        return Err(Error::InvalidInput);
    }
    Ok(())
}
