// SPDX-License-Identifier: MPL-2.0

//! On-disk special inode types and Linux device-number encoding.

use crate::{Error, Inode, Result};

/// A device number representable by ext4's twelve-bit major and twenty-bit minor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceNumber {
    major: u32,
    minor: u32,
}

impl DeviceNumber {
    pub fn new(major: u32, minor: u32) -> Result<Self> {
        if major > 0xfff || minor > 0xfffff {
            return Err(Error::InvalidInput);
        }
        Ok(Self { major, minor })
    }
    pub fn major(&self) -> u32 {
        self.major
    }
    pub fn minor(&self) -> u32 {
        self.minor
    }
    pub(super) fn encoded(&self) -> u32 {
        (self.major << 8) | (self.minor & 0xff) | ((self.minor & 0xfff00) << 12)
    }
}

/// A special inode that has no ordinary file data.
#[derive(Clone, Copy, Debug)]
pub enum SpecialFile {
    NamedPipe,
    Socket,
    Character(DeviceNumber),
    Block(DeviceNumber),
}

impl SpecialFile {
    pub(super) fn mode(self) -> u16 {
        match self {
            Self::NamedPipe => 0x1000,
            Self::Socket => 0xc000,
            Self::Character(_) => 0x2000,
            Self::Block(_) => 0x6000,
        }
    }
}

impl Inode {
    pub fn device_number(&self) -> Option<DeviceNumber> {
        if !matches!(self.mode() & 0xf000, 0x2000 | 0x6000) {
            return None;
        }
        let old = u32::from_le_bytes(self.raw[40..44].try_into().unwrap());
        let new = u32::from_le_bytes(self.raw[44..48].try_into().unwrap());
        let (major, minor) = if old != 0 {
            ((old >> 8) & 0xff, old & 0xff)
        } else {
            ((new >> 8) & 0xfff, (new & 0xff) | ((new >> 12) & 0xfff00))
        };
        Some(DeviceNumber { major, minor })
    }

    pub(super) fn is_special(&self) -> bool {
        matches!(self.mode() & 0xf000, 0x1000 | 0x2000 | 0x6000 | 0xc000)
    }

    pub(super) fn directory_type(&self) -> Result<u8> {
        match self.mode() & 0xf000 {
            0x8000 => Ok(1),
            0x4000 => Ok(2),
            0x2000 => Ok(3),
            0x6000 => Ok(4),
            0x1000 => Ok(5),
            0xc000 => Ok(6),
            0xa000 => Ok(7),
            _ => Err(Error::Corrupt),
        }
    }
}
