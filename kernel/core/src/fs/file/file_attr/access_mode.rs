// SPDX-License-Identifier: MPL-2.0

use aster_rights::Rights;

#[expect(non_camel_case_types)]
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessMode {
    /// Read only
    O_RDONLY = 0,
    /// Write only
    O_WRONLY = 1,
    /// Read write
    O_RDWR = 2,
    /// Checks read/write permission but permits only descriptor control operations.
    IoctlOnly = 3,
}

impl AccessMode {
    pub(crate) fn is_readable(&self) -> bool {
        matches!(*self, AccessMode::O_RDONLY | AccessMode::O_RDWR)
    }

    pub(crate) fn is_writable(&self) -> bool {
        matches!(*self, AccessMode::O_WRONLY | AccessMode::O_RDWR)
    }
}

impl AccessMode {
    pub(crate) const MASK: u32 = 0b11;

    pub(crate) fn from_u32(flags: u32) -> Self {
        let bits = (flags & Self::MASK) as u8;
        match bits {
            0 => Self::O_RDONLY,
            1 => Self::O_WRONLY,
            2 => Self::O_RDWR,
            _ => Self::IoctlOnly,
        }
    }
}

impl From<Rights> for AccessMode {
    fn from(rights: Rights) -> Self {
        match (
            rights.contains(Rights::READ),
            rights.contains(Rights::WRITE),
        ) {
            (true, true) => AccessMode::O_RDWR,
            (true, false) => AccessMode::O_RDONLY,
            (false, true) => AccessMode::O_WRONLY,
            // The file is opened with `O_PATH`. We follow Linux to report `O_RDONLY` here (e.g.,
            // in `/proc/[pid]/fdinfo/[n]`).
            (false, false) => AccessMode::O_RDONLY,
        }
    }
}

impl From<AccessMode> for Rights {
    fn from(access_mode: AccessMode) -> Rights {
        match access_mode {
            AccessMode::O_RDONLY => Rights::READ,
            AccessMode::O_WRONLY => Rights::WRITE,
            AccessMode::O_RDWR => Rights::READ | Rights::WRITE,
            AccessMode::IoctlOnly => Rights::empty(),
        }
    }
}
