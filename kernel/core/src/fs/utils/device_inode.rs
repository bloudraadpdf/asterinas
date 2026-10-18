// SPDX-License-Identifier: MPL-2.0

//! Opening filesystem pipes and registered device nodes.

use crate::{
    device,
    fs::{
        file::{AccessMode, PerOpenFileOps, StatusFlags},
        pipe::Pipe,
        vfs::inode::Inode,
    },
    prelude::*,
};

pub(in crate::fs) fn open(
    inode: &dyn Inode,
    pipe: Option<&Pipe>,
    access: AccessMode,
    flags: StatusFlags,
) -> Option<Result<Box<dyn PerOpenFileOps>>> {
    if let Some(pipe) = pipe {
        return Some(pipe.open_named(access, flags));
    }
    let type_ = inode.type_().device_type()?;
    Some(inode.metadata().and_then(|metadata| {
        metadata
            .self_dev_id
            .and_then(|id| device::lookup(type_, id))
            .ok_or_else(|| Error::new(Errno::ENODEV))?
            .open()
    }))
}
