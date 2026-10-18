// SPDX-License-Identifier: MPL-2.0

//! Opening filesystem device nodes through the registered guest drivers.

use crate::{
    device,
    fs::{file::PerOpenFileOps, vfs::inode::Inode},
    prelude::*,
};

pub(in crate::fs) fn open(inode: &dyn Inode) -> Option<Result<Box<dyn PerOpenFileOps>>> {
    let type_ = inode.type_().device_type()?;
    Some(inode.metadata().and_then(|metadata| {
        metadata
            .self_dev_id
            .and_then(|id| device::lookup(type_, id))
            .ok_or_else(|| Error::new(Errno::ENODEV))?
            .open()
    }))
}
