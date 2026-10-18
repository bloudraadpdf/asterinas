// SPDX-License-Identifier: MPL-2.0

//! Inline and block-backed symbolic links.

use alloc::{vec, vec::Vec};

use io_util::ReadAt;

use crate::{Error, Ext4, Geometry, Inode, Result};

impl<D: ReadAt> Ext4<D> {
    /// Reads a symbolic link's byte target without requiring UTF-8.
    pub fn read_link(&self, inode: &Inode) -> Result<Vec<u8>> {
        if inode.mode() & 0xf000 != 0xa000 {
            return Err(Error::InvalidInput);
        }
        if inode.size() >= u64::from(self.geometry.block_size()) {
            return Err(Error::Corrupt);
        }
        let mut target = vec![0; inode.size() as usize];
        if inode.is_inline_symlink(&self.geometry)? {
            if target.len() > 60 {
                return Err(Error::Corrupt);
            }
            target.copy_from_slice(&inode.raw[40..40 + inode.size() as usize]);
        } else if self.read_at(inode, 0, &mut target)? != target.len() {
            return Err(Error::Corrupt);
        }
        if target.contains(&0) {
            return Err(Error::Corrupt);
        }
        Ok(target)
    }
}

impl Inode {
    pub(super) fn is_inline_symlink(&self, geometry: &Geometry) -> Result<bool> {
        if self.mode() & 0xf000 != 0xa000 {
            return Ok(false);
        }
        Ok(!self.has_data_allocations(geometry)?)
    }
}
