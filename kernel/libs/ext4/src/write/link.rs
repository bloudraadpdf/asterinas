// SPDX-License-Identifier: MPL-2.0

//! Atomic hard links, including relinking a live unlinked inode.

use io_util::WriteAt;

use super::{Timestamp, WritableExt4};
use crate::{Error, Inode, Result, directory, u32_at};

impl<D: WriteAt> WritableExt4<D> {
    /// Adds a regular file's name in a linear directory.
    pub fn link(
        &mut self,
        parent: u32,
        name: &[u8],
        number: u32,
        changed: Timestamp,
    ) -> Result<Inode> {
        directory::check_child_name(name)?;
        self.update(|transaction| {
            let mut parent = transaction.directory_for_update(parent)?;
            let mut inode = transaction.mutable_inode(number)?;
            if inode.mode() & 0xf000 == 0x4000 {
                return Err(Error::PermissionDenied);
            }
            if u32_at(&inode.raw, 32)? & 0x30 != 0 {
                return Err(Error::PermissionDenied);
            }
            transaction.ensure_missing(&parent, name)?;
            let links = inode
                .links()
                .checked_add(1)
                .filter(|count| *count <= 65000)
                .ok_or(Error::TooManyLinks)?;
            if inode.links() == 0 {
                if !transaction
                    .view()
                    .orphans(transaction.limits.orphan_inodes)?
                    .iter()
                    .any(|entry| entry.number == number)
                {
                    return Err(Error::Corrupt);
                }
                transaction.remove_orphan(&mut inode)?;
            }
            inode.raw[26..28].copy_from_slice(&links.to_le_bytes());
            let file_type = inode.directory_type()?;
            transaction.insert_entry(&mut parent, name, number, file_type)?;
            transaction.store_entry_change(parent, inode, changed)
        })
    }
}
