// SPDX-License-Identifier: MPL-2.0

use crate::{prelude::*, process::Pid};

/// `struct __user_cap_header_struct` in Linux.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.18.6/source/include/uapi/linux/capability.h#L40>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(crate) struct CUserCapHeader {
    pub version: u32,
    pub pid: Pid,
}

/// `struct __user_cap_data_struct` in Linux.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.18.6/source/include/uapi/linux/capability.h#L40>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(crate) struct CUserCapData {
    pub effective: u32,
    pub permitted: u32,
    pub inheritable: u32,
}

pub(crate) const LINUX_CAPABILITY_VERSION_1: u32 = 0x19980330;
pub(crate) const LINUX_CAPABILITY_VERSION_2: u32 = 0x20071026;
pub(crate) const LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;

impl CUserCapHeader {
    pub(crate) fn data_words(version: u32) -> Option<usize> {
        match version {
            LINUX_CAPABILITY_VERSION_1 => Some(1),
            LINUX_CAPABILITY_VERSION_2 | LINUX_CAPABILITY_VERSION_3 => Some(2),
            _ => None,
        }
    }
}
