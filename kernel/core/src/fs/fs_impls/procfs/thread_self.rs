// SPDX-License-Identifier: MPL-2.0

use alloc::format;

use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcSym, ProcSymOps},
        vfs::inode::{Inode, SymbolicLink},
    },
    prelude::*,
    process::{PidNamespace, posix_thread::AsPosixThread},
};

/// Represents the inode at `/proc/self-thread`.
pub(super) struct ThreadSelfSymOps(Arc<PidNamespace>);

impl ThreadSelfSymOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/thread_self.c#L50>
        let namespace = super::ProcFs::namespace_of(&parent);
        ProcSym::new(Self(namespace), parent, mkmod!(a+rwx))
    }
}

impl ProcSymOps for ThreadSelfSymOps {
    fn read_link(&self) -> Result<SymbolicLink> {
        let pid = current!().pid_in(&self.0);
        let tid = current_thread!().as_posix_thread().unwrap().tid_in(&self.0);
        if pid == 0 || tid == 0 {
            return_errno_with_message!(Errno::ENOENT, "the thread is outside this PID namespace");
        }
        Ok(SymbolicLink::Plain(format!("{}/task/{}", pid, tid)))
    }
}
