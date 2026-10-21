// SPDX-License-Identifier: MPL-2.0

//! A bounded, read-only EROFS image reader.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{vec, vec::Vec};

use io_util::{IoError, ReadAt, checksum};

mod compression;
mod xattr;

/// A filesystem or device error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The underlying input failed.
    Io(IoError),
    /// The image contains an invalid record or checksum.
    Corrupt,
    /// The image requires an unsupported format feature.
    Unsupported,
    /// The requested directory entry does not exist.
    NotFound,
    /// The supplied output buffer is too small.
    BufferTooSmall,
}

impl From<IoError> for Error {
    fn from(error: IoError) -> Self {
        Self::Io(error)
    }
}

type Result<T> = core::result::Result<T, Error>;

/// An EROFS inode, decoded without retaining a disk buffer.
#[derive(Clone, Debug)]
pub struct Inode {
    pub nid: u64,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub links: u32,
    pub mtime_seconds: u64,
    pub mtime_nanoseconds: u32,
    offset: u64,
    metadata_size: u64,
    inode_size: u64,
    layout: u8,
    data: u32,
}

impl Inode {
    /// Returns the Linux device major and minor for a character or block inode.
    pub fn device_number(&self) -> Option<(u32, u32)> {
        matches!(self.mode & 0xf000, 0x2000 | 0x6000).then(|| {
            (
                (self.data >> 8) & 0xfff,
                (self.data & 0xff) | ((self.data >> 12) & 0xfff00),
            )
        })
    }
}

/// One directory entry, retaining the on-disk byte name.
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub nid: u64,
    pub name: Vec<u8>,
    pub file_type: u8,
    pub next_offset: u64,
}

/// An immutable EROFS volume backed by byte-addressed reads.
pub struct Erofs<D> {
    device: D,
    block_size: u64,
    image_size: u64,
    meta_offset: u64,
    root_nid: u64,
    inodes: u64,
    epoch: u64,
    nanoseconds: u32,
    incompatible: u32,
    xattr_offset: u64,
    decoder: compression::Decoder,
}

impl<D: ReadAt> Erofs<D> {
    /// Opens an image after checking its superblock and root inode.
    pub fn open(device: D) -> Result<Self> {
        let mut superblock = [0; 128];
        device.read_exact_at(1024, &mut superblock)?;
        if le32(&superblock, 0)? != 0xe0f5_e1e2 {
            return Err(Error::Corrupt);
        }
        let bits = superblock[12];
        if !(9..=12).contains(&bits) || superblock[13] != 0 {
            return Err(Error::Unsupported);
        }
        let incompatible = le32(&superblock, 80)?;
        // Static single-device images with LZ4 and compact compression indexes.
        if incompatible & !1 != 0 || le16(&superblock, 86)? != 0 {
            return Err(Error::Unsupported);
        }
        let block_size = 1u64 << bits;
        let image_size = u64::from(le32(&superblock, 36)?) * block_size;
        let meta_offset = u64::from(le32(&superblock, 40)?) * block_size;
        if image_size > device.len() || image_size < 1024 + 128 || meta_offset >= image_size {
            return Err(Error::Corrupt);
        }
        let nanoseconds = le32(&superblock, 32)?;
        if nanoseconds >= 1_000_000_000 {
            return Err(Error::Corrupt);
        }
        if le32(&superblock, 8)? & 1 != 0 {
            let checksum_size = if block_size > 1024 {
                block_size - 1024
            } else {
                block_size
            };
            let mut block = vec![0; checksum_size as usize];
            device.read_exact_at(1024, &mut block)?;
            let expected = le32(&block, 4)?;
            block[4..8].fill(0);
            if checksum::crc32c(!0, &block) != expected {
                return Err(Error::Corrupt);
            }
        }
        let volume = Self {
            device,
            block_size,
            image_size,
            meta_offset,
            root_nid: u64::from(le16(&superblock, 14)?),
            inodes: le64(&superblock, 16)?,
            epoch: le64(&superblock, 24)?,
            nanoseconds,
            incompatible,
            xattr_offset: u64::from(le32(&superblock, 44)?) * block_size,
            decoder: compression::Decoder::default(),
        };
        if volume.inode(volume.root_nid)?.mode & 0xf000 != 0x4000 {
            return Err(Error::Corrupt);
        }
        Ok(volume)
    }

    pub fn root_nid(&self) -> u64 {
        self.root_nid
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    pub fn image_size(&self) -> u64 {
        self.image_size
    }

    pub fn inode_count(&self) -> u64 {
        self.inodes
    }

    fn check_range(&self, offset: u64, length: usize) -> Result<()> {
        if offset > self.image_size || length as u64 > self.image_size - offset {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn read(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        self.check_range(offset, output.len())?;
        self.device.read_exact_at(offset, output)?;
        Ok(())
    }

    fn read_metadata(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        self.check_range(offset, output.len())?;
        self.device.read_metadata_exact_at(offset, output)?;
        Ok(())
    }

    /// Reads and validates one inode.
    pub fn inode(&self, nid: u64) -> Result<Inode> {
        let offset = nid
            .checked_mul(32)
            .and_then(|value| value.checked_add(self.meta_offset))
            .ok_or(Error::Corrupt)?;
        let mut raw = [0; 64];
        self.read_metadata(offset, &mut raw[..32])?;
        let format = le16(&raw, 0)?;
        if format & !0x1f != 0 {
            return Err(Error::Unsupported);
        }
        let layout = ((format >> 1) & 7) as u8;
        if layout > 3 {
            return Err(Error::Unsupported);
        }
        let extended = format & 1 != 0;
        let inode_size = if extended { 64 } else { 32 };
        if extended {
            self.read_metadata(offset + 32, &mut raw[32..])?;
        }
        let xattrs = u64::from(le16(&raw, 2)?);
        let metadata_size = inode_size
            + if xattrs == 0 {
                0
            } else {
                12 + (xattrs - 1) * 4
            };
        let (size, uid, gid, seconds, nanoseconds, links) = if extended {
            (
                le64(&raw, 8)?,
                le32(&raw, 24)?,
                le32(&raw, 28)?,
                le64(&raw, 32)?,
                le32(&raw, 40)?,
                le32(&raw, 44)?,
            )
        } else {
            (
                u64::from(le32(&raw, 8)?),
                u32::from(le16(&raw, 24)?),
                u32::from(le16(&raw, 26)?),
                self.epoch
                    .checked_add(u64::from(le32(&raw, 12)?))
                    .ok_or(Error::Corrupt)?,
                self.nanoseconds,
                if format & 16 != 0 && le16(&raw, 4)? & 0xf000 != 0x4000 {
                    1
                } else {
                    u32::from(le16(&raw, 6)?)
                },
            )
        };
        if nanoseconds >= 1_000_000_000
            || size > i64::MAX as u64
            || offset
                .checked_add(metadata_size)
                .is_none_or(|end| end > self.image_size)
        {
            return Err(Error::Corrupt);
        }
        Ok(Inode {
            nid,
            mode: le16(&raw, 4)?,
            uid,
            gid,
            size,
            links,
            mtime_seconds: seconds,
            mtime_nanoseconds: nanoseconds,
            offset,
            metadata_size,
            inode_size,
            layout,
            data: le32(&raw, 16)?,
        })
    }

    /// Returns the inode's allocated size in 512-byte sectors, as reported by Linux.
    pub fn allocated_sectors(&self, inode: &Inode) -> u64 {
        let blocks = if matches!(inode.layout, 1 | 3) {
            u64::from(inode.data)
        } else {
            inode.size.div_ceil(self.block_size)
        };
        blocks * (self.block_size / 512)
    }

    /// Reads file bytes, returning zero at end of file.
    pub fn read_at(&mut self, inode: &Inode, offset: u64, output: &mut [u8]) -> Result<usize> {
        if offset >= inode.size || output.is_empty() {
            return Ok(0);
        }
        let count = (inode.size - offset).min(output.len() as u64) as usize;
        let output = &mut output[..count];
        if matches!(inode.layout, 1 | 3) {
            self.read_compressed(inode, offset, output)?;
        } else {
            let tail_offset = inode.size / self.block_size * self.block_size;
            let mut copied = 0;
            while copied < output.len() {
                let position = offset + copied as u64;
                let (physical, available) = if inode.layout == 2 && position >= tail_offset {
                    let inline = inode
                        .offset
                        .checked_add(inode.metadata_size)
                        .ok_or(Error::Corrupt)?;
                    let physical = inline
                        .checked_add(position - tail_offset)
                        .ok_or(Error::Corrupt)?;
                    if physical / self.block_size != inline / self.block_size {
                        return Err(Error::Corrupt);
                    }
                    (physical, self.block_size - physical % self.block_size)
                } else {
                    (
                        (u64::from(inode.data) * self.block_size)
                            .checked_add(position)
                            .ok_or(Error::Corrupt)?,
                        if inode.layout == 2 {
                            tail_offset - position
                        } else {
                            inode.size - position
                        },
                    )
                };
                let length = available.min((output.len() - copied) as u64) as usize;
                if length == 0 {
                    return Err(Error::Corrupt);
                }
                if inode.mode & 0xf000 == 0x4000 {
                    self.read_metadata(physical, &mut output[copied..copied + length])?;
                } else {
                    self.read(physical, &mut output[copied..copied + length])?;
                }
                copied += length;
            }
        }
        Ok(count)
    }

    /// Reads the next directory entry without collecting the directory in RAM.
    pub fn next_entry(&mut self, inode: &Inode, offset: u64) -> Result<Option<DirEntry>> {
        if inode.mode & 0xf000 != 0x4000 {
            return Err(Error::Corrupt);
        }
        if offset >= inode.size {
            return Ok(None);
        }
        let block_offset = offset / self.block_size * self.block_size;
        let mut block = vec![0; (inode.size - block_offset).min(self.block_size) as usize];
        self.read_at(inode, block_offset, &mut block)?;
        let first_name = usize::from(le16(&block, 8)?);
        if first_name == 0 || !first_name.is_multiple_of(12) || first_name > block.len() {
            return Err(Error::Corrupt);
        }
        let index = (offset - block_offset) as usize;
        if !index.is_multiple_of(12) || index >= first_name {
            return Err(Error::Corrupt);
        }
        let name_start = usize::from(le16(&block, index + 8)?);
        let next_index = index + 12;
        let name_end = if next_index < first_name {
            usize::from(le16(&block, next_index + 8)?)
        } else {
            block.len()
        };
        if name_start < first_name || name_end <= name_start || name_end > block.len() {
            return Err(Error::Corrupt);
        }
        let name = &block[name_start..name_end];
        let length = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        let name = &name[..length];
        if name.is_empty()
            || name.len() > 255
            || name.contains(&b'/')
            || (next_index < first_name && length != name_end - name_start)
        {
            return Err(Error::Corrupt);
        }
        let file_type = block[index + 10];
        if file_type > 7 || block[index + 11] != 0 {
            return Err(Error::Corrupt);
        }
        Ok(Some(DirEntry {
            nid: le64(&block, index)?,
            name: name.to_vec(),
            file_type,
            next_offset: block_offset
                + if next_index < first_name {
                    next_index as u64
                } else {
                    self.block_size
                },
        }))
    }

    pub fn lookup(&mut self, parent: &Inode, name: &[u8]) -> Result<Inode> {
        let mut offset = 0;
        while let Some(entry) = self.next_entry(parent, offset)? {
            if entry.name == name {
                return self.inode(entry.nid);
            }
            offset = entry.next_offset;
        }
        Err(Error::NotFound)
    }
}

fn le16(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset.checked_add(2).ok_or(Error::Corrupt)?)
            .ok_or(Error::Corrupt)?
            .try_into()
            .map_err(|_| Error::Corrupt)?,
    ))
}

fn le32(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset.checked_add(4).ok_or(Error::Corrupt)?)
            .ok_or(Error::Corrupt)?
            .try_into()
            .map_err(|_| Error::Corrupt)?,
    ))
}

fn le64(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset.checked_add(8).ok_or(Error::Corrupt)?)
            .ok_or(Error::Corrupt)?
            .try_into()
            .map_err(|_| Error::Corrupt)?,
    ))
}
