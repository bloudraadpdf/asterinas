// SPDX-License-Identifier: MPL-2.0

//! This module offers `/proc/loadavg` file support, which tells the user space
//! about the cpu load average for the last 1, 5, and 15 minutes.
//!
//! Reference: <https://www.man7.org/linux/man-pages/man5/proc_loadavg.5.html>

use aster_util::printer::VmPrinter;

use crate::{
    fs::{
        file::mkmod,
        procfs::template::{ProcFile, ProcFileOps},
        vfs::inode::Inode,
    },
    prelude::*,
    process::PidNamespace,
    sched::{self, loadavg::get_loadavg},
};

/// Represents the inode at `/proc/loadavg`.
pub(super) struct LoadAvgFileOps(Arc<PidNamespace>);

impl LoadAvgFileOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference:
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/loadavg.c#L33>
        // <https://elixir.bootlin.com/linux/v6.16.5/source/fs/proc/generic.c#L549-L550>
        let namespace = super::ProcFs::namespace_of(&parent);
        ProcFile::new(Self(namespace), parent, mkmod!(a+r))
    }
}

impl ProcFileOps for LoadAvgFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);

        let avg = get_loadavg();
        let (nr_queued, nr_running) = sched::nr_queued_and_running();
        writeln!(
            printer,
            "{:.2} {:.2} {:.2} {}/{} {}",
            avg[0],
            avg[1],
            avg[2],
            nr_running,
            nr_queued,
            self.0.last_pid(),
        )?;

        Ok(printer.bytes_written())
    }
}
