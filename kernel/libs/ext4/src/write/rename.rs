// SPDX-License-Identifier: MPL-2.0

//! Atomic rename with replacement and bounded directory ancestry checks.

use alloc::collections::BTreeSet;

use io_util::{ReadAt, WriteAt};

use super::{Timestamp, Transaction, WritableExt4};
use crate::{Error, Inode, Result, directory, u32_at};

impl<D: WriteAt> WritableExt4<D> {
    /// Moves a name between directories, retaining a replaced inode for open references.
    pub fn rename(
        &mut self,
        old_parent: u32,
        old_name: &[u8],
        new_parent: u32,
        new_name: &[u8],
        changed: Timestamp,
    ) -> Result<Option<Inode>> {
        directory::check_child_name(old_name)?;
        directory::check_child_name(new_name)?;
        self.update(|transaction| {
            let mut old_parent = transaction.directory_for_update(old_parent)?;
            let mut destination = if new_parent == old_parent.number() {
                None
            } else {
                Some(transaction.directory_for_update(new_parent)?)
            };
            if u32_at(&old_parent.raw, 32)? & 0x20 != 0 {
                return Err(Error::PermissionDenied);
            }
            let source = transaction.view().lookup(&old_parent, old_name)?;
            let mut source = transaction.mutable_inode(source.number())?;
            if u32_at(&source.raw, 32)? & 0x30 != 0 {
                return Err(Error::PermissionDenied);
            }
            if source.links() == 0 {
                return Err(Error::Corrupt);
            }
            let target_parent = destination.as_ref().unwrap_or(&old_parent);
            let mut replaced = match transaction.view().lookup(target_parent, new_name) {
                Ok(inode) if inode.number() == source.number() => return Ok(None),
                Ok(inode) => Some(transaction.mutable_inode(inode.number())?),
                Err(Error::NotFound) => None,
                Err(error) => return Err(error),
            };
            if replaced.is_some() && u32_at(&target_parent.raw, 32)? & 0x20 != 0 {
                return Err(Error::PermissionDenied);
            }
            let is_directory = source.mode() & 0xf000 == 0x4000;
            if let Some(target) = &replaced {
                match (is_directory, target.mode() & 0xf000 == 0x4000) {
                    (false, true) => return Err(Error::IsDirectory),
                    (true, false) => return Err(Error::NotDirectory),
                    _ => (),
                }
            }
            if is_directory && destination.is_some() {
                transaction.check_directory_move(source.number(), new_parent)?;
                let parent = transaction.view().lookup(&source, b"..")?;
                if parent.number() != old_parent.number() {
                    return Err(Error::Corrupt);
                }
                if source.flags() & 0x1000 != 0 {
                    transaction.set_indexed_parent(&source, old_parent.number(), new_parent)?;
                } else {
                    transaction.remove_entry(&source, b"..", old_parent.number())?;
                    transaction.insert_entry(&mut source, b"..", new_parent, 2)?;
                }
                transaction.change_directory_links(&mut old_parent, -1)?;
            }
            transaction.remove_entry(&old_parent, old_name, source.number())?;
            let moving_parent = destination.is_some();
            let target_parent = destination.as_mut().unwrap_or(&mut old_parent);
            if let Some(target) = &mut replaced {
                transaction.drop_link(target, target_parent)?;
                transaction.remove_entry(target_parent, new_name, target.number())?;
                changed.store(target, 12, 132)?;
                transaction.store_inode(target.clone())?;
            }
            if is_directory && moving_parent {
                transaction.change_directory_links(target_parent, 1)?;
            }
            let file_type = source.directory_type()?;
            transaction.insert_entry(target_parent, new_name, source.number(), file_type)?;
            changed.store(target_parent, 16, 136)?;
            changed.store(target_parent, 12, 132)?;
            transaction.store_inode(target_parent.clone())?;
            if destination.is_some() {
                changed.store(&mut old_parent, 16, 136)?;
                changed.store(&mut old_parent, 12, 132)?;
                transaction.store_inode(old_parent)?;
            }
            changed.store(&mut source, 12, 132)?;
            transaction.store_inode(source)?;
            Ok(replaced)
        })
    }
}

impl<D: ReadAt> Transaction<'_, D> {
    fn check_directory_move(&self, number: u32, mut parent: u32) -> Result<()> {
        let mut seen = BTreeSet::new();
        for _ in 0..self.limits.directory_depth {
            if number == parent {
                return Err(Error::InvalidInput);
            }
            if parent == 2 {
                return Ok(());
            }
            if !seen.insert(parent) {
                return Err(Error::Corrupt);
            }
            let directory = self.view().inode(parent)?;
            parent = self.view().lookup(&directory, b"..")?.number();
        }
        Err(Error::ResourceLimit)
    }
}
