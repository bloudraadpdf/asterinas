// SPDX-License-Identifier: MPL-2.0

//! Atomic ownership, permission and timestamp updates.

use io_util::WriteAt;

use super::{Timestamp, WritableExt4};
use crate::{Error, Result};

/// Inode fields to change together without changing file type or allocation.
#[derive(Clone, Copy, Debug)]
pub struct MetadataUpdate {
    pub permissions: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub accessed: Option<Timestamp>,
    pub modified: Option<Timestamp>,
    pub changed: Timestamp,
}

impl MetadataUpdate {
    pub fn new(changed: Timestamp) -> Self {
        Self {
            permissions: None,
            uid: None,
            gid: None,
            accessed: None,
            modified: None,
            changed,
        }
    }
}

impl<D: WriteAt> WritableExt4<D> {
    /// Commits the supplied metadata fields and change time in one transaction.
    pub fn set_metadata(&mut self, number: u32, update: MetadataUpdate) -> Result<()> {
        self.update(|transaction| {
            let mut inode = transaction.metadata_inode(number)?;
            if inode.flags() & 0x30 != 0 {
                return Err(Error::PermissionDenied);
            }
            if let Some(permissions) = update.permissions {
                if permissions & !0x0fff != 0 {
                    return Err(Error::InvalidInput);
                }
                let mode = (inode.mode() & 0xf000) | permissions;
                inode.raw[..2].copy_from_slice(&mode.to_le_bytes());
            }
            if let Some(uid) = update.uid {
                inode.raw[2..4].copy_from_slice(&(uid as u16).to_le_bytes());
                inode.raw[120..122].copy_from_slice(&((uid >> 16) as u16).to_le_bytes());
            }
            if let Some(gid) = update.gid {
                inode.raw[24..26].copy_from_slice(&(gid as u16).to_le_bytes());
                inode.raw[122..124].copy_from_slice(&((gid >> 16) as u16).to_le_bytes());
            }
            if let Some(accessed) = update.accessed {
                accessed.store(&mut inode, 8, 140)?;
            }
            if let Some(modified) = update.modified {
                modified.store(&mut inode, 16, 136)?;
            }
            update.changed.store(&mut inode, 12, 132)?;
            transaction.store_inode(inode)
        })
    }
}
