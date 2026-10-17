// SPDX-License-Identifier: MPL-2.0

use crate::{
    fs::{file::InodeMode, utils},
    prelude::*,
    process::{Gid, Uid},
};

pub(super) struct MountOptions {
    pub(super) pages: usize,
    pub(super) inodes: usize,
    pub(super) mode: InodeMode,
    pub(super) uid: Uid,
    pub(super) gid: Gid,
}

impl MountOptions {
    pub(super) fn parse(args: Option<&str>) -> Result<Self> {
        let (uid, gid) = utils::current_fs_ids();
        let mut options = Self {
            pages: (crate::vm::mem_total() / PAGE_SIZE / 2).max(1),
            inodes: (crate::vm::mem_total() / PAGE_SIZE / 2).max(1),
            mode: InodeMode::from_bits_truncate(0o1777),
            uid,
            gid,
        };
        for option in args.unwrap_or("").split(',').filter(|s| !s.is_empty()) {
            let (name, value) = option.split_once('=').ok_or_else(invalid_option)?;
            match name {
                "size" => options.pages = parse_size(value)?.div_ceil(PAGE_SIZE),
                "nr_blocks" => options.pages = parse_size(value)?,
                "nr_inodes" => options.inodes = parse_size(value)?,
                "mode" => {
                    let mode = u16::from_str_radix(value, 8).map_err(|_| invalid_option())?;
                    options.mode = InodeMode::from_bits_truncate(mode & 0o7777);
                }
                "uid" => options.uid = Uid::new(parse_id(value)?),
                "gid" => options.gid = Gid::new(parse_id(value)?),
                _ => return Err(invalid_option()),
            }
        }
        Ok(options)
    }
}

fn parse_id(value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|id| *id != u32::MAX)
        .ok_or_else(invalid_option)
}

fn parse_size(value: &str) -> Result<usize> {
    let (digits, shift) = match value.as_bytes().last() {
        Some(b'k' | b'K') => (&value[..value.len() - 1], 10),
        Some(b'm' | b'M') => (&value[..value.len() - 1], 20),
        Some(b'g' | b'G') => (&value[..value.len() - 1], 30),
        _ => (value, 0),
    };
    digits
        .parse::<usize>()
        .ok()
        .and_then(|size| size.checked_mul(1usize << shift))
        .ok_or_else(invalid_option)
}

fn invalid_option() -> Error {
    Error::with_message(Errno::EINVAL, "invalid tmpfs mount option")
}
