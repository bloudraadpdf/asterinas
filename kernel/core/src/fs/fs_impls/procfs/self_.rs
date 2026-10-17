// SPDX-License-Identifier: MPL-2.0

use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcSym, ProcSymOps},
        vfs::inode::{Inode, SymbolicLink},
    },
    prelude::*,
    process::PidNamespace,
};

/// Represents the inode at `/proc/self`.
pub(super) struct SelfSymOps(Arc<PidNamespace>);

impl SelfSymOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/self.c#L50>
        let namespace = super::ProcFs::namespace_of(&parent);
        ProcSym::new(Self(namespace), parent, mkmod!(a+rwx))
    }
}

impl ProcSymOps for SelfSymOps {
    fn read_link(&self) -> Result<SymbolicLink> {
        let pid = current!().pid_in(&self.0);
        if pid == 0 {
            return_errno_with_message!(Errno::ENOENT, "the process is outside this PID namespace");
        }
        Ok(SymbolicLink::Plain(pid.to_string()))
    }
}
