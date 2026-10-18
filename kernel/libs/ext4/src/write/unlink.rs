// SPDX-License-Identifier: MPL-2.0

//! Unlink with deferred reclamation after the last open reference.

use io_util::{ReadAt, WriteAt};

use super::{NewInodeKind, Timestamp, Transaction, WritableExt4};
use crate::{Error, Inode, Result, directory, u16_at, u32_at};

impl<D: WriteAt> WritableExt4<D> {
    /// Removes a regular file's name, retaining the inode for open references.
    pub fn unlink(&mut self, parent: u32, name: &[u8], changed: Timestamp) -> Result<Inode> {
        self.remove_child(parent, name, changed, NewInodeKind::File)
    }

    /// Removes an empty directory, retaining its inode for open references.
    pub fn remove_directory(
        &mut self,
        parent: u32,
        name: &[u8],
        changed: Timestamp,
    ) -> Result<Inode> {
        self.remove_child(parent, name, changed, NewInodeKind::Directory)
    }

    fn remove_child(
        &mut self,
        parent: u32,
        name: &[u8],
        changed: Timestamp,
        kind: NewInodeKind,
    ) -> Result<Inode> {
        directory::check_child_name(name)?;
        self.update(|transaction| {
            let mut parent = transaction.directory_for_update(parent)?;
            if u32_at(&parent.raw, 32)? & 0x20 != 0 {
                return Err(Error::PermissionDenied);
            }
            let number = transaction.view().lookup(&parent, name)?.number();
            let mut inode = transaction.mutable_inode(number)?;
            match kind {
                NewInodeKind::File if inode.mode() & 0xf000 == 0x4000 => {
                    return Err(Error::IsDirectory);
                }
                NewInodeKind::Directory if inode.mode() & 0xf000 != 0x4000 => {
                    return Err(Error::NotDirectory);
                }
                _ => (),
            }
            transaction.drop_link(&mut inode, &mut parent)?;
            transaction.remove_entry(&parent, name, number)?;
            transaction.store_entry_change(parent, inode, changed)
        })
    }

    /// Reclaims an unlinked inode after the VFS releases its last reference.
    pub fn reap(&mut self, number: u32) -> Result<()> {
        self.check_health()?;
        let inode = self.fs.inode(number)?;
        self.fs.check_reclaimable(&inode)?;
        let attribute_block = inode.attribute_block_number()?;
        if attribute_block != 0 && self.reserved.contains(attribute_block) {
            return Err(Error::Corrupt);
        }
        if inode.links() != 0
            || !self
                .fs
                .orphans(self.limits.orphan_inodes)?
                .iter()
                .any(|entry| entry.number == number)
        {
            return Err(Error::InvalidInput);
        }
        self.run_cleanup(|session| {
            session.update(|transaction| {
                let mut inode = transaction.mutable_inode(number)?;
                inode.set_size(0);
                transaction.store_inode(inode)
            })?;
            session.finish_truncate(number)?;
            session.update(|transaction| {
                let mut inode = transaction.view().inode(number)?;
                transaction.remove_orphan(&mut inode)?;
                transaction.release_attributes(&mut inode)?;
                transaction.free_inode(inode)
            })
        })
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    pub(super) fn drop_link(&mut self, inode: &mut Inode, parent: &mut Inode) -> Result<()> {
        self.view().check_reclaimable(inode)?;
        self.check_attribute_block(inode)?;
        if u32_at(&inode.raw, 32)? & 0x30 != 0 {
            return Err(Error::PermissionDenied);
        }
        let links = if inode.mode() & 0xf000 == 0x4000 {
            self.check_empty_directory(inode, parent.number())?;
            self.change_directory_links(parent, -1)?;
            0
        } else {
            inode.links().checked_sub(1).ok_or(Error::Corrupt)?
        };
        inode.raw[26..28].copy_from_slice(&links.to_le_bytes());
        if links == 0 {
            self.add_orphan(inode)?;
        }
        Ok(())
    }

    fn check_empty_directory(&self, inode: &Inode, parent: u32) -> Result<()> {
        if inode.mode() & 0xf000 != 0x4000 {
            return Err(Error::NotDirectory);
        }
        let mut dot = false;
        let mut dotdot = false;
        for entry in self.view().read_dir(inode)? {
            let entry = entry?;
            match entry.name() {
                b"." if !dot && entry.inode() == inode.number() => dot = true,
                b".." if !dotdot && entry.inode() == parent => dotdot = true,
                b"." | b".." => return Err(Error::Corrupt),
                _ => return Err(Error::NotEmpty),
            }
        }
        if !dot
            || !dotdot
            || !(inode.links() == 2
                || inode.links() == 1 && self.geometry.has_large_directory_link_counts())
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    pub(super) fn remove_entry(&mut self, parent: &Inode, name: &[u8], number: u32) -> Result<()> {
        let view = self.view();
        let entry = view.find_entry(parent, name)?;
        if entry.inode() != number {
            return Err(Error::Corrupt);
        }
        let block_size = u64::from(self.geometry.block_size());
        let logical =
            u32::try_from((entry.next_offset() - 1) / block_size).map_err(|_| Error::Corrupt)?;
        let physical = view.file_block(parent, logical)?.ok_or(Error::Corrupt)?;
        self.check_file_block(physical)?;
        let mut raw = view.read_block(physical)?;
        let end = ((entry.next_offset() - 1) % block_size + 1) as usize;
        let mut offset = 0;
        let mut previous = None;
        while offset < end {
            let length = usize::from(u16_at(&raw, offset + 4)?);
            if length < 8 || length > end - offset || !length.is_multiple_of(4) {
                return Err(Error::Corrupt);
            }
            if offset + length == end {
                if let Some(previous) = previous {
                    raw[previous + 4..previous + 6]
                        .copy_from_slice(&((end - previous) as u16).to_le_bytes());
                    raw[offset..end].fill(0);
                } else {
                    raw[offset..offset + 4].fill(0);
                    raw[offset + 6..end].fill(0);
                }
                self.set_directory_checksum(parent, &mut raw)?;
                self.metadata_block(physical)?.copy_from_slice(&raw);
                return Ok(());
            }
            previous = Some(offset);
            offset += length;
        }
        Err(Error::Corrupt)
    }
}
