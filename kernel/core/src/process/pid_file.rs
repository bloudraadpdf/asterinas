// SPDX-License-Identifier: MPL-2.0

use core::fmt::Display;

use crate::{
    events::IoEvents,
    fs::{
        file::{AccessMode, CreationFlags, FileCommon, FileLike, StatusFlags, file_table::FdFlags},
        pseudofs::PidfdFs,
    },
    prelude::*,
    process::{
        PidNamespace, Process,
        signal::{PollHandle, Pollable},
    },
};

pub(crate) struct PidFile {
    process: Weak<Process>,
    common: FileCommon,
}

impl Debug for PidFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let pid = self.process.upgrade().map_or(u32::MAX, |p| p.pid());
        f.debug_struct("PidFile")
            .field("process", &pid)
            .field("is_nonblocking", &self.common.is_nonblocking())
            .finish_non_exhaustive()
    }
}

impl PidFile {
    pub(crate) fn new(process: Arc<Process>, is_nonblocking: bool) -> Self {
        let pseudo_path = PidfdFs::new_path(|_| "anon_inode:[pidfd]".to_string());
        let status_flags = if is_nonblocking {
            StatusFlags::O_NONBLOCK
        } else {
            StatusFlags::empty()
        };

        Self {
            process: Arc::downgrade(&process),
            // Reference: <https://elixir.bootlin.com/linux/v7.0/source/kernel/fork.c#L1898>.
            common: FileCommon::new(pseudo_path, AccessMode::O_RDWR, status_flags),
        }
    }

    fn check_io_events(&self) -> IoEvents {
        // "A PID file descriptor can be monitored using poll(2), select(2),
        // and epoll(7).  When the process that it refers to terminates, these
        // interfaces indicate the file descriptor as readable."
        // Reference: <https://man7.org/linux/man-pages/man2/pidfd_open.2.html>.
        let Some(process) = self.process.upgrade() else {
            // The process has been reaped.
            return IoEvents::IN | IoEvents::HUP;
        };
        if process.status().is_zombie() {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    pub(super) fn is_nonblocking(&self) -> bool {
        self.common.is_nonblocking()
    }

    pub(crate) fn process_opt(&self) -> Option<Arc<Process>> {
        self.process.upgrade()
    }
}

impl FileLike for PidFile {
    fn read_at(&self, _offset: usize, _writer: &mut VmWriter) -> Result<usize> {
        return_errno_with_message!(
            Errno::EINVAL,
            "PID file cannot be read at a specific offset"
        );
    }

    fn write_at(&self, _offset: usize, _reader: &mut VmReader) -> Result<usize> {
        return_errno_with_message!(
            Errno::EINVAL,
            "PID file cannot be written at a specific offset"
        );
    }

    fn common(&self) -> &FileCommon {
        &self.common
    }

    fn dump_proc_fdinfo(
        self: Arc<Self>,
        fd_flags: FdFlags,
        namespace: &PidNamespace,
    ) -> Box<dyn Display> {
        struct FdInfo {
            flags: u32,
            pid: i32,
            namespace_pids: Vec<i32>,
        }

        impl Display for FdInfo {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                writeln!(f, "pos:\t{}", 0)?;
                writeln!(f, "flags:\t0{:o}", self.flags)?;
                writeln!(f, "mnt_id:\t{}", PidfdFs::mount_node().id())?;
                writeln!(f, "ino:\t{}", PidfdFs::shared_inode().ino())?;
                writeln!(f, "Pid:\t{}", self.pid)?;
                write!(f, "NSpid:")?;
                for pid in &self.namespace_pids {
                    write!(f, "\t{pid}")?;
                }
                writeln!(f)
            }
        }

        let mut flags = self.common.status_flags().bits() | self.common.access_mode() as u32;
        if fd_flags.contains(FdFlags::CLOEXEC) {
            flags |= CreationFlags::O_CLOEXEC.bits();
        }
        let (pid, namespace_pids) = self.process.upgrade().map_or_else(
            || (-1, vec![-1]),
            |process| {
                let pid = process.pid_in(namespace).cast_signed();
                let ids = process
                    .pid_identity()
                    .namespace_ids_from(namespace)
                    .into_iter()
                    .map(|pid| pid.cast_signed())
                    .collect();
                (pid, ids)
            },
        );

        Box::new(FdInfo {
            flags,
            pid,
            namespace_pids,
        })
    }
}

impl Pollable for PidFile {
    fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        let Some(process) = self.process.upgrade() else {
            // The process has been reaped.
            return mask & (IoEvents::IN | IoEvents::HUP);
        };
        process
            .pidfile_pollee
            .poll_with(mask, poller, || self.check_io_events())
    }
}
