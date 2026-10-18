// SPDX-License-Identifier: MPL-2.0

use alloc::vec;

use io_util::ReadAt;

use super::{Erofs, Error, Inode, Result, le16, le32};

impl<D: ReadAt> Erofs<D> {
    /// Reads an attribute by its complete name, or reports its size for an empty buffer.
    pub fn get_xattr(&self, inode: &Inode, name: &[u8], output: &mut [u8]) -> Result<usize> {
        let mut result = Err(Error::NotFound);
        self.visit_xattrs(inode, |attribute, value_offset, value_size| {
            if attribute != name {
                return Ok(true);
            }
            if !output.is_empty() {
                if output.len() < value_size {
                    return Err(Error::BufferTooSmall);
                }
                self.read(value_offset, &mut output[..value_size])?;
            }
            result = Ok(value_size);
            Ok(false)
        })?;
        result
    }

    /// Visits attribute names without reading their values.
    pub fn list_xattrs(
        &self,
        inode: &Inode,
        mut visit: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        self.visit_xattrs(inode, |name, _, _| {
            visit(name)?;
            Ok(true)
        })
    }

    fn visit_xattrs(
        &self,
        inode: &Inode,
        mut visit: impl FnMut(&[u8], u64, usize) -> Result<bool>,
    ) -> Result<()> {
        if inode.metadata_size == inode.inode_size {
            return Ok(());
        }
        let start = inode.offset + inode.inode_size;
        let end = inode.offset + inode.metadata_size;
        let mut header = [0; 12];
        self.read(start, &mut header)?;
        let shared_count = u64::from(header[4]);
        let entries_start = start + 12 + shared_count * 4;
        if entries_start > end {
            return Err(Error::Corrupt);
        }
        for index in 0..shared_count {
            let mut address = [0; 4];
            self.read(start + 12 + index * 4, &mut address)?;
            let offset = self.xattr_offset + u64::from(le32(&address, 0)?) * 4;
            if !self.visit_xattr(offset, self.image_size, &mut visit)?.1 {
                return Ok(());
            }
        }
        let mut offset = entries_start;
        while offset < end {
            let (next, proceed) = self.visit_xattr(offset, end, &mut visit)?;
            if !proceed {
                return Ok(());
            }
            offset = next;
        }
        Ok(())
    }

    fn visit_xattr(
        &self,
        offset: u64,
        end: u64,
        visit: &mut impl FnMut(&[u8], u64, usize) -> Result<bool>,
    ) -> Result<(u64, bool)> {
        let mut header = [0; 4];
        if offset.checked_add(4).is_none_or(|limit| limit > end) {
            return Err(Error::Corrupt);
        }
        self.read(offset, &mut header)?;
        let name_size = usize::from(header[0]);
        let value_size = usize::from(le16(&header, 2)?);
        let entry_size = (4 + name_size + value_size).next_multiple_of(4) as u64;
        let next = offset.checked_add(entry_size).ok_or(Error::Corrupt)?;
        if next > end {
            return Err(Error::Corrupt);
        }
        let prefix: &[u8] = match header[1] {
            1 => b"user.",
            2 => b"system.posix_acl_access",
            3 => b"system.posix_acl_default",
            4 => b"trusted.",
            6 => b"security.",
            _ => return Err(Error::Unsupported),
        };
        if prefix.len() + name_size > 255 {
            return Err(Error::Corrupt);
        }
        let mut name = vec![0; prefix.len() + name_size];
        name[..prefix.len()].copy_from_slice(prefix);
        self.read(offset + 4, &mut name[prefix.len()..])?;
        if name.contains(&0) {
            return Err(Error::Corrupt);
        }
        let proceed = visit(&name, offset + 4 + name_size as u64, value_size)?;
        Ok((next, proceed))
    }
}
