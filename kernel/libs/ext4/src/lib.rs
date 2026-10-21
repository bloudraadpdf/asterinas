// SPDX-License-Identifier: MPL-2.0

//! Checked ext4 disk metadata and transactions.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{vec, vec::Vec};

use io_util::{IoError, ReadAt, checksum};
use smallvec::{SmallVec, smallvec};

mod attribute;
mod device;
mod directory;
mod extent;
mod geometry;
mod journal;
mod metadata;
mod orphan;
mod recovery;
mod symlink;
mod write;

pub use attribute::Attribute;
pub use device::{DeviceNumber, SpecialFile};
pub use directory::{DirEntry, Directory};
pub use geometry::Geometry;
pub use journal::{Journal, JournalChecksum, JournalTag};
pub use write::{
    AttributeMode, CreateMetadata, MetadataUpdate, MountLimits, Timestamp, WritableExt4,
};

/// An ext4 metadata or I/O error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// An underlying I/O operation failed.
    Io(IoError),
    /// A disk record has an invalid range, structure or checksum.
    Corrupt,
    /// A disk feature is not supported.
    Unsupported,
    /// The volume requires journal replay before access.
    NeedsRecovery,
    /// An operation exceeds the caller's resource limits.
    ResourceLimit,
    /// A mutation was requested on a read-only device.
    ReadOnly,
    /// A write session failed and must be closed before recovery.
    Aborted,
    /// A supplied value cannot be represented by ext4.
    InvalidInput,
    /// An inode is not a directory.
    NotDirectory,
    /// A regular-file operation was requested on a directory.
    IsDirectory,
    /// A directory still contains live children.
    NotEmpty,
    /// An inode does not contain the requested extended attribute.
    NoAttribute,
    /// A directory does not contain the requested name.
    NotFound,
    /// A name or symbolic-link target exceeds its format limit.
    NameTooLong,
    /// Allocation space or a directory's index capacity is exhausted.
    NoSpace,
    /// A directory already contains the requested name.
    AlreadyExists,
    /// An inode flag forbids the operation.
    PermissionDenied,
    /// A link count cannot be represented by the enabled disk features.
    TooManyLinks,
}

impl From<IoError> for Error {
    fn from(error: IoError) -> Self {
        Self::Io(error)
    }
}

type Result<T> = core::result::Result<T, Error>;

/// An ext4 volume with validated geometry.
pub struct Ext4<D> {
    device: D,
    geometry: Geometry,
}

impl<D: ReadAt> Ext4<D> {
    /// Opens and checks a clean volume's superblock.
    pub fn open(device: D) -> Result<Self> {
        let mut raw = [0; 1024];
        device.read_exact_at(1024, &mut raw)?;
        let geometry = Geometry::parse(raw, device.len())?;
        if geometry.needs_recovery()
            || geometry.orphan_cleanup_required()
            || geometry.last_orphan() != 0
            || (!geometry.is_clean() && geometry.journal_inode() == 0)
        {
            return Err(Error::NeedsRecovery);
        }
        Ok(Self { device, geometry })
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    fn read_block(&self, block: u64) -> Result<Vec<u8>> {
        let mut data = vec![0; self.geometry.block_size() as usize];
        self.device
            .read_metadata_exact_at(self.geometry.block_offset(block)?, &mut data)?;
        Ok(data)
    }

    fn read_bitmap(&self, block: u64, bits: u32, expected: u32) -> Result<Vec<u8>> {
        let raw = self.read_block(block)?;
        if self.geometry.has_metadata_checksums() {
            let bytes = raw
                .get(..(bits as usize).div_ceil(8))
                .ok_or(Error::Corrupt)?;
            let crc = self.device.metadata_crc32c(
                self.geometry.block_offset(block)?,
                bytes,
                self.geometry.checksum_seed(),
            );
            let mask = if self.geometry.descriptor_size() >= 64 {
                u32::MAX
            } else {
                u32::from(u16::MAX)
            };
            if crc & mask != expected & mask {
                return Err(Error::Corrupt);
            }
        }
        Ok(raw)
    }

    fn read_data_block(&self, block: u64) -> Result<Vec<u8>> {
        let mut data = vec![0; self.geometry.block_size() as usize];
        self.device
            .read_exact_at(self.geometry.block_offset(block)?, &mut data)?;
        Ok(data)
    }

    /// Reads and checks the descriptor for one allocation group.
    pub fn group(&self, index: u32) -> Result<Group> {
        if index >= self.geometry.groups() {
            return Err(Error::Corrupt);
        }
        let mut raw = smallvec![0; self.geometry.descriptor_size() as usize];
        let offset = self.geometry.descriptor_offset(index)?;
        self.device.read_metadata_exact_at(offset, &mut raw)?;
        if self.geometry.has_metadata_checksums() {
            let expected = u16_at(&raw, 30)?;
            raw[30..32].fill(0);
            let crc = checksum::crc32c(self.geometry.checksum_seed(), &index.to_le_bytes());
            if checksum::crc32c(crc, &raw) as u16 != expected {
                return Err(Error::Corrupt);
            }
            raw[30..32].copy_from_slice(&expected.to_le_bytes());
        }
        let address = |low: usize, high: usize| -> Result<u64> {
            let upper = if raw.len() >= 64 {
                u64::from(u32_at(&raw, high)?) << 32
            } else {
                0
            };
            Ok(u64::from(u32_at(&raw, low)?) | upper)
        };
        let group = Group {
            block_bitmap: address(0, 32)?,
            inode_bitmap: address(4, 36)?,
            inode_table: address(8, 40)?,
            free_blocks: u32::from(u16_at(&raw, 12)?)
                | if raw.len() >= 64 {
                    u32::from(u16_at(&raw, 44)?) << 16
                } else {
                    0
                },
            free_inodes: u32::from(u16_at(&raw, 14)?)
                | if raw.len() >= 64 {
                    u32::from(u16_at(&raw, 46)?) << 16
                } else {
                    0
                },
            flags: u16_at(&raw, 18)?,
            block_bitmap_checksum: u32::from(u16_at(&raw, 24)?)
                | if raw.len() >= 64 {
                    u32::from(u16_at(&raw, 56)?) << 16
                } else {
                    0
                },
            inode_bitmap_checksum: u32::from(u16_at(&raw, 26)?)
                | if raw.len() >= 64 {
                    u32::from(u16_at(&raw, 58)?) << 16
                } else {
                    0
                },
            raw,
        };
        let inode_blocks = (u64::from(self.geometry.inodes_per_group())
            * u64::from(self.geometry.inode_size()))
        .div_ceil(u64::from(self.geometry.block_size()));
        self.geometry.block_offset(group.block_bitmap)?;
        self.geometry.block_offset(group.inode_bitmap)?;
        self.geometry.block_offset(group.inode_table)?;
        if group.block_bitmap == group.inode_bitmap
            || group
                .inode_table
                .checked_add(inode_blocks)
                .is_none_or(|end| end > self.geometry.blocks())
            || u64::from(group.free_blocks)
                > (self.geometry.blocks() - self.geometry.group_start(index))
                    .min(u64::from(self.geometry.blocks_per_group()))
            || u64::from(group.free_inodes)
                > (u64::from(self.geometry.inodes())
                    - u64::from(index) * u64::from(self.geometry.inodes_per_group()))
                .min(u64::from(self.geometry.inodes_per_group()))
        {
            return Err(Error::Corrupt);
        }
        let table = group.inode_table..group.inode_table + inode_blocks;
        if table.contains(&group.block_bitmap) || table.contains(&group.inode_bitmap) {
            return Err(Error::Corrupt);
        }
        Ok(group)
    }

    /// Reads and checks an allocated inode record.
    pub fn inode(&self, number: u32) -> Result<Inode> {
        if number == 0 || number > self.geometry.inodes() {
            return Err(Error::Corrupt);
        }
        let group_index = (number - 1) / self.geometry.inodes_per_group();
        let index = (number - 1) % self.geometry.inodes_per_group();
        let group = self.group(group_index)?;
        if group.flags & 1 != 0 {
            return Err(Error::Corrupt);
        }
        let bitmap = self.read_bitmap(
            group.inode_bitmap,
            self.geometry.inodes_per_group(),
            group.inode_bitmap_checksum,
        )?;
        if bitmap[index as usize / 8] & (1 << (index % 8)) == 0 {
            return Err(Error::Corrupt);
        }
        self.read_inode_record(number, &group)
    }

    fn read_inode_record(&self, number: u32, group: &Group) -> Result<Inode> {
        if number == 0 || number > self.geometry.inodes() {
            return Err(Error::Corrupt);
        }
        let index = (number - 1) % self.geometry.inodes_per_group();
        let mut raw = smallvec![0; self.geometry.inode_size() as usize];
        let offset = self
            .geometry
            .block_offset(group.inode_table)?
            .checked_add(u64::from(index) * u64::from(self.geometry.inode_size()))
            .ok_or(Error::Corrupt)?;
        self.device.read_metadata_exact_at(offset, &mut raw)?;
        let extra = if raw.len() > 128 {
            usize::from(u16_at(&raw, 128)?)
        } else {
            0
        };
        if extra > raw.len() - 128 || !extra.is_multiple_of(4) {
            return Err(Error::Corrupt);
        }
        let generation = u32_at(&raw, 100)?;
        if self.geometry.has_metadata_checksums() {
            let expected = u32::from(u16_at(&raw, 124)?)
                | if extra >= 4 {
                    u32::from(u16_at(&raw, 130)?) << 16
                } else {
                    0
                };
            raw[124..126].fill(0);
            if extra >= 4 {
                raw[130..132].fill(0);
            }
            let crc = checksum::crc32c(self.geometry.checksum_seed(), &number.to_le_bytes());
            let crc = checksum::crc32c(crc, &generation.to_le_bytes());
            let mask = if extra >= 4 {
                u32::MAX
            } else {
                u32::from(u16::MAX)
            };
            if checksum::crc32c(crc, &raw) & mask != expected {
                return Err(Error::Corrupt);
            }
            raw[124..126].copy_from_slice(&(expected as u16).to_le_bytes());
            if extra >= 4 {
                raw[130..132].copy_from_slice(&((expected >> 16) as u16).to_le_bytes());
            }
        }
        Ok(Inode { number, raw })
    }
}

/// A checked allocation-group descriptor.
#[derive(Clone, Debug)]
pub struct Group {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    free_blocks: u32,
    free_inodes: u32,
    flags: u16,
    block_bitmap_checksum: u32,
    inode_bitmap_checksum: u32,
    raw: SmallVec<[u8; 64]>,
}

impl Group {
    pub fn block_bitmap(&self) -> u64 {
        self.block_bitmap
    }
    pub fn inode_bitmap(&self) -> u64 {
        self.inode_bitmap
    }
    pub fn inode_table(&self) -> u64 {
        self.inode_table
    }
    pub fn free_blocks(&self) -> u32 {
        self.free_blocks
    }
    pub fn free_inodes(&self) -> u32 {
        self.free_inodes
    }
    pub fn block_bitmap_checksum(&self) -> u32 {
        self.block_bitmap_checksum
    }
}

/// A checked allocated inode record.
#[derive(Clone, Debug)]
pub struct Inode {
    number: u32,
    raw: SmallVec<[u8; 256]>,
}

impl Inode {
    pub fn number(&self) -> u32 {
        self.number
    }
    pub fn mode(&self) -> u16 {
        u16::from_le_bytes([self.raw[0], self.raw[1]])
    }
    pub fn size(&self) -> u64 {
        u64::from(u32::from_le_bytes(self.raw[4..8].try_into().unwrap()))
            | (u64::from(u32::from_le_bytes(self.raw[108..112].try_into().unwrap())) << 32)
    }
    pub fn links(&self) -> u16 {
        u16::from_le_bytes([self.raw[26], self.raw[27]])
    }
    pub fn uid(&self) -> u32 {
        u32::from(u16::from_le_bytes([self.raw[2], self.raw[3]]))
            | (u32::from(u16::from_le_bytes([self.raw[120], self.raw[121]])) << 16)
    }
    pub fn gid(&self) -> u32 {
        u32::from(u16::from_le_bytes([self.raw[24], self.raw[25]]))
            | (u32::from(u16::from_le_bytes([self.raw[122], self.raw[123]])) << 16)
    }
}

fn bytes_at<const N: usize>(raw: &[u8], offset: usize) -> Result<[u8; N]> {
    raw.get(offset..offset.checked_add(N).ok_or(Error::Corrupt)?)
        .ok_or(Error::Corrupt)?
        .try_into()
        .map_err(|_| Error::Corrupt)
}
fn u16_at(raw: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes_at(raw, offset)?))
}
fn u32_at(raw: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes_at(raw, offset)?))
}
