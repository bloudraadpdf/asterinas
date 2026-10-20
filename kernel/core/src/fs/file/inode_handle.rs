// SPDX-License-Identifier: MPL-2.0

//! Opened Inode-backed File Handle

use core::fmt::Display;

use super::{
    AccessMode, CreationFlags, FileCommon, FileLike, InodeType, MappableObject,
    SettableStatusFlags, StatusFlags, SyncMode, file_table::FdFlags,
};
use crate::{
    events::IoEvents,
    fs::{
        utils::DirentVisitor,
        vfs::{
            inode::{FallocMode, FileOps},
            path::Path,
            xattr::clear_file_priv,
        },
    },
    prelude::*,
    process::{
        PidNamespace,
        signal::{PollHandle, Pollable},
    },
    util::{MultiRead, VmReaderArray, VmWriterArray, ioctl::RawIoctl},
    vm::vmar::FileMmapRequest,
};

pub(crate) struct InodeHandle {
    /// `open_file` is similar to the `file_private` field in Linux's `file` structure. If
    /// `open_file` is `Some(_)`, typical file operations including `read`, `write`, `poll`,
    /// and `ioctl` will be provided by the per-open file object instead of `path`.
    open_file: Option<Box<dyn PerOpenFileOps>>,
    offset: Mutex<usize>,
    /// Path and common states of the handle.
    //
    // This field is placed last so that its `Path` keeps the corresponding filesystem alive
    // while `open_file` is dropped, because releasing `open_file` may access that filesystem.
    //
    // Struct fields are dropped in declaration order.
    // Reference: <https://doc.rust-lang.org/reference/destructors.html>.
    common: FileCommon,
}

impl InodeHandle {
    pub(crate) fn new(
        path: Path,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Result<Self> {
        let inode = path.inode();
        if !status_flags.contains(StatusFlags::O_PATH) {
            // "Opening a file or directory with the O_PATH flag requires no permissions on the
            // object itself".
            // Reference: <https://man7.org/linux/man-pages/man2/openat.2.html>
            inode.check_permission(access_mode.into())?;
        }

        Self::new_unchecked_access(path, access_mode, status_flags)
    }

    pub(crate) fn new_unchecked_access(
        path: Path,
        mut access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Result<Self> {
        let inode = path.inode();
        let open_file = if status_flags.contains(StatusFlags::O_PATH) {
            // The file is opened with `O_PATH`. We follow Linux to report `O_RDONLY` here (e.g.,
            // in `/proc/[pid]/fdinfo/[n]`).
            access_mode = AccessMode::O_RDONLY;
            None
        } else if inode.type_() == InodeType::Socket {
            return_errno_with_message!(Errno::ENXIO, "a socket inode cannot be opened as a file");
        } else if inode.type_() == InodeType::Dir && access_mode.is_writable() {
            return_errno_with_message!(Errno::EISDIR, "a directory cannot be opened writable");
        } else {
            inode
                .open(path.dentry(), access_mode, status_flags)
                .transpose()?
        };

        Ok(Self {
            open_file,
            offset: Mutex::new(0),
            common: FileCommon::new(path, access_mode, status_flags),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.common.path()
    }

    pub(crate) fn access_mode(&self) -> AccessMode {
        self.common.access_mode()
    }

    pub(crate) fn status_flags(&self) -> StatusFlags {
        self.common.status_flags()
    }

    pub(crate) fn offset(&self) -> usize {
        let offset = self.offset.lock();
        *offset
    }

    fn file_ops_and_is_offset_aware(&self) -> (&dyn FileOps, bool) {
        if let Some(ref open_file) = self.open_file {
            let is_offset_aware = open_file.is_offset_aware();
            return (open_file.as_ref(), is_offset_aware);
        }

        let inode = self.path().inode();
        let is_offset_aware = inode.type_().is_seekable();
        (inode.as_ref(), is_offset_aware)
    }

    /// Returns the `FileOps` for positional I/O, rejecting files
    /// that do not support `pread`/`pwrite`.
    fn file_ops_for_positional_io(&self) -> Result<&dyn FileOps> {
        if let Some(ref open_file) = self.open_file {
            open_file.check_positional_io()?;
            return Ok(open_file.as_ref());
        }

        let inode = self.path().inode();
        if !inode.type_().is_seekable() {
            return_errno_with_message!(
                Errno::ESPIPE,
                "the inode cannot be read or written at a specific offset"
            );
        }
        Ok(inode.as_ref())
    }

    pub(crate) fn readdir(&self, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.status_flags().contains(StatusFlags::O_PATH) || !self.access_mode().is_readable() {
            return_errno_with_message!(Errno::EBADF, "the file is not opened readable");
        }

        let file_ops: &dyn FileOps = if let Some(ref open_file) = self.open_file {
            open_file.as_ref()
        } else {
            self.path().inode().as_ref()
        };
        let mut offset = self.offset.lock();
        let read_cnt = file_ops.readdir_at(
            &crate::fs::vfs::inode::ReaddirContext::new(self.path().dentry(), self.status_flags()),
            *offset,
            visitor,
        )?;
        *offset += read_cnt;
        Ok(read_cnt)
    }

    fn read_with_offset(
        &self,
        explicit: Option<usize>,
        operation: impl FnOnce(&dyn FileOps, usize, StatusFlags) -> Result<usize>,
    ) -> Result<usize> {
        let (file_ops, current_offset) = if explicit.is_some() {
            (self.file_ops_for_positional_io()?, false)
        } else {
            self.file_ops_and_is_offset_aware()
        };
        if self.status_flags().contains(StatusFlags::O_PATH) || !self.access_mode().is_readable() {
            return_errno_with_message!(Errno::EBADF, "the file is not opened readable");
        }
        let flags = self.status_flags();
        if !current_offset {
            return operation(file_ops, explicit.unwrap_or(0), flags);
        }
        let mut offset = self.offset.lock();
        let count = operation(file_ops, *offset, flags)?;
        *offset += count;
        Ok(count)
    }

    fn write_with_offset(
        &self,
        explicit: Option<usize>,
        has_data: bool,
        operation: impl FnOnce(&dyn FileOps, usize, StatusFlags) -> Result<usize>,
    ) -> Result<usize> {
        let (file_ops, current_offset) = if explicit.is_some() {
            (self.file_ops_for_positional_io()?, false)
        } else {
            self.file_ops_and_is_offset_aware()
        };
        if self.status_flags().contains(StatusFlags::O_PATH) || !self.access_mode().is_writable() {
            return_errno_with_message!(Errno::EBADF, "the file is not opened writable");
        }
        if has_data {
            clear_file_priv(self.path().dentry())?;
        }
        let flags = self.status_flags();
        let append = flags.contains(StatusFlags::O_APPEND) && self.open_file.is_none();
        if !current_offset {
            let position = if append {
                self.path().size()
            } else {
                explicit.unwrap_or(0)
            };
            return operation(file_ops, position, flags);
        }
        let mut offset = self.offset.lock();
        if append {
            *offset = self.path().size();
        }
        let count = operation(file_ops, *offset, flags)?;
        *offset += count;
        Ok(count)
    }

    pub(crate) fn downcast_open_file<T: 'static>(&self) -> Result<Option<&T>> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        let Some(open_file) = self.open_file.as_ref() else {
            return Ok(None);
        };

        Ok((open_file.as_ref() as &dyn Any).downcast_ref::<T>())
    }
}

impl Pollable for InodeHandle {
    fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        if let Some(ref open_file) = self.open_file {
            return open_file.poll(mask, poller);
        }

        if self.status_flags().contains(StatusFlags::O_PATH) {
            IoEvents::NVAL
        } else {
            let events = IoEvents::IN | IoEvents::OUT;
            events & mask
        }
    }
}

impl FileLike for InodeHandle {
    fn read(&self, writer: &mut VmWriter) -> Result<usize> {
        self.read_with_offset(None, |file, offset, flags| {
            file.read_at(offset, writer, flags)
        })
    }

    fn readv(&self, writers: &mut VmWriterArray) -> Result<usize> {
        self.read_with_offset(None, |file, offset, flags| {
            file.readv_at(offset, writers.writers_mut(), flags)
        })
    }

    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        self.read_with_offset(Some(offset), |file, offset, flags| {
            file.read_at(offset, writer, flags)
        })
    }

    fn readv_at(&self, offset: usize, writers: &mut VmWriterArray) -> Result<usize> {
        self.read_with_offset(Some(offset), |file, offset, flags| {
            file.readv_at(offset, writers.writers_mut(), flags)
        })
    }

    fn write(&self, reader: &mut VmReader) -> Result<usize> {
        self.write_with_offset(None, reader.has_remain(), |file, offset, flags| {
            file.write_at(offset, reader, flags)
        })
    }

    fn writev(&self, readers: &mut VmReaderArray) -> Result<usize> {
        self.write_with_offset(None, !readers.is_empty(), |file, offset, flags| {
            file.writev_at(offset, readers.readers_mut(), flags)
        })
    }

    fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        self.write_with_offset(Some(offset), reader.has_remain(), |file, offset, flags| {
            file.write_at(offset, reader, flags)
        })
    }

    fn writev_at(&self, offset: usize, readers: &mut VmReaderArray) -> Result<usize> {
        self.write_with_offset(Some(offset), !readers.is_empty(), |file, offset, flags| {
            file.writev_at(offset, readers.readers_mut(), flags)
        })
    }

    fn ioctl(&self, raw_ioctl: RawIoctl) -> Result<i32> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        if let Some(ref open_file) = self.open_file {
            return open_file.ioctl(self.common.path(), raw_ioctl);
        }

        return_errno_with_message!(Errno::ENOTTY, "ioctl is not supported");
    }

    fn mappable(&self, request: FileMmapRequest) -> Result<MappableObject<'_>> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        if let Some(ref open_file) = self.open_file {
            // If the file is a special file (e.g., device file), we should
            // return the file-specific mappable object.
            return open_file.mappable(request);
        }

        let inode = self.path().inode();
        if let Some(page_cache) = inode.page_cache() {
            // Otherwise, if the inode has a page cache, it is a file-backed
            // mapping and we return the VMO as the mappable object.
            Ok(MappableObject::Vmo(page_cache))
        } else {
            return_errno_with_message!(Errno::ENODEV, "the file is not mappable");
        }
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }
        if !self.access_mode().is_writable() {
            return_errno_with_message!(Errno::EINVAL, "the file is not opened writable");
        }

        // `ftruncate` uses the descriptor's write mode and must not recheck current inode
        // permissions. See <https://man7.org/linux/man-pages/man2/truncate.2.html>.
        self.path().resize_unchecked_access(new_size)
    }

    fn settable_status_flags(&self) -> SettableStatusFlags {
        if let Some(ref open_file) = self.open_file {
            return open_file.settable_status_flags();
        }

        if self.path().inode().type_().is_regular_file() {
            // FIXME: This operation should depend on whether the underlying filesystem supports `O_DIRECT`.
            SettableStatusFlags::minimal().with_o_direct()
        } else {
            SettableStatusFlags::minimal()
        }
    }

    fn seek(&self, pos: SeekFrom) -> Result<usize> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        if let Some(ref open_file) = self.open_file {
            open_file.check_seekable()?;
            if open_file.is_offset_aware() {
                return do_seek_util(&self.offset, pos, open_file.seek_end()?);
            } else {
                return Ok(0);
            }
        }

        let inode = self.path().inode();
        if !inode.type_().is_seekable() {
            return_errno_with_message!(Errno::ESPIPE, "seek is not supported");
        }
        do_seek_util(&self.offset, pos, inode.seek_end())
    }

    fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        if self.status_flags().contains(StatusFlags::O_PATH) || !self.access_mode().is_writable() {
            return_errno_with_message!(Errno::EBADF, "the file is not opened writable");
        }

        let dentry = self.path().dentry();
        let inode = dentry.inode().as_ref();
        let inode_type = inode.type_();

        // TODO: `fallocate` on pipe files also fails with `ESPIPE`.
        if inode_type == InodeType::NamedPipe {
            return_errno_with_message!(Errno::ESPIPE, "the inode is a FIFO file");
        }
        if !(inode_type == InodeType::File || inode_type == InodeType::Dir) {
            return_errno_with_message!(
                Errno::ENODEV,
                "the inode is not a regular file or a directory"
            );
        }

        let status_flags = self.status_flags();
        if status_flags.contains(StatusFlags::O_APPEND)
            && (mode == FallocMode::PunchHoleKeepSize
                || mode == FallocMode::CollapseRange
                || mode == FallocMode::InsertRange)
        {
            return_errno_with_message!(
                Errno::EPERM,
                "the flags do not work on the append-only file"
            );
        }
        if status_flags.contains(StatusFlags::O_DIRECT)
            || status_flags.contains(StatusFlags::O_PATH)
        {
            return_errno_with_message!(
                Errno::EBADF,
                "currently fallocate file with O_DIRECT or O_PATH is not supported"
            );
        }

        clear_file_priv(dentry)?;
        inode.fallocate(mode, offset, len)
    }

    fn common(&self) -> &FileCommon {
        &self.common
    }

    fn dump_proc_fdinfo(
        self: Arc<Self>,
        fd_flags: FdFlags,
        _namespace: &PidNamespace,
    ) -> Box<dyn Display> {
        struct FdInfo {
            inner: Arc<InodeHandle>,
            fd_flags: FdFlags,
        }

        impl Display for FdInfo {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                let mut flags = self.inner.status_flags().bits() | self.inner.access_mode() as u32;
                if self.fd_flags.contains(FdFlags::CLOEXEC) {
                    flags |= CreationFlags::O_CLOEXEC.bits();
                }

                writeln!(f, "pos:\t{}", self.inner.offset())?;
                writeln!(f, "flags:\t0{:o}", flags)?;
                writeln!(f, "mnt_id:\t{}", self.inner.path().mount_node().id())?;
                writeln!(f, "ino:\t{}", self.inner.path().inode().ino())
            }
        }

        Box::new(FdInfo {
            inner: self,
            fd_flags,
        })
    }

    fn sync(&self, mode: SyncMode) -> Result<()> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        if let Some(ref open_file) = self.open_file {
            return open_file.sync(mode);
        }

        self.path().sync(mode)
    }
}

impl Debug for InodeHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("InodeHandle")
            .field("path", &self.path())
            .field("offset", &self.offset())
            .field("status_flags", &self.status_flags())
            .finish_non_exhaustive()
    }
}

/// Describes the position to seek from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeekFrom {
    Start(usize),
    End(isize),
    Current(isize),
}

/// File operations for one opened file description.
///
/// A per-open file object can hold file-description-specific state and override
/// operations that are not purely inode-backed, such as state and operations for
/// devices, pipes, namespace files, and procfs files.
pub trait PerOpenFileOps: Pollable + FileOps + Any + Send + Sync + 'static {
    /// Checks whether the `seek()` operation should fail.
    fn check_seekable(&self) -> Result<()>;

    /// Returns whether the `read()`/`write()` operation should use and advance the offset.
    ///
    /// If [`PerOpenFileOps::check_seekable`] succeeds but this method returns `false`,
    /// the offset in the `seek()` operation will be ignored.
    /// In that case, the `seek()` operation will do nothing but succeed.
    fn is_offset_aware(&self) -> bool;

    /// Checks whether positional I/O (`pread`/`pwrite`) is supported.
    ///
    /// The default delegates to [`check_seekable`], which is correct for
    /// most files. Override this for files that support positional I/O
    /// but not seeking (e.g., nsfs).
    ///
    /// [`check_seekable`]: PerOpenFileOps::check_seekable
    fn check_positional_io(&self) -> Result<()> {
        self.check_seekable()
    }

    /// Returns the end position for [`SeekFrom::End`].
    ///
    /// This is intentionally separate from `Inode::seek_end`. Both `Inode`
    /// and [`PerOpenFileOps`] need `SEEK_END` support, but `Inode::seek_end`
    /// has an inode-specific default implementation, so the two cannot be
    /// cleanly unified under [`FileOps`].
    fn seek_end(&self) -> Result<Option<usize>> {
        Ok(None)
    }

    // See `FileLike::mappable`.
    fn mappable(&self, _request: FileMmapRequest) -> Result<MappableObject<'_>> {
        return_errno_with_message!(Errno::EINVAL, "the file is not mappable");
    }

    fn ioctl(&self, _path: &Path, _raw_ioctl: RawIoctl) -> Result<i32> {
        return_errno_with_message!(Errno::ENOTTY, "ioctl is not supported");
    }

    /// Returns the status flags that can be set for this opened file.
    fn settable_status_flags(&self) -> SettableStatusFlags {
        // `O_ASYNC` and `O_DIRECT` can only be set on file descriptions that explicitly
        // support them.
        SettableStatusFlags::minimal()
    }

    /// Synchronizes the file according to `mode`.
    ///
    /// Per-open file operations do not support synchronization by default.
    /// Implementations that support synchronization must override this method.
    fn sync(&self, _mode: SyncMode) -> Result<()> {
        return_errno_with_message!(Errno::EINVAL, "the file does not support synchronization")
    }
}

fn do_seek_util(offset: &Mutex<usize>, pos: SeekFrom, end: Option<usize>) -> Result<usize> {
    let mut offset = offset.lock();

    let new_offset = match pos {
        SeekFrom::Start(off) => off,
        SeekFrom::End(diff) => {
            if let Some(end) = end {
                end.wrapping_add_signed(diff)
            } else {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "seeking the file from the end is not supported"
                );
            }
        }
        SeekFrom::Current(diff) => offset.wrapping_add_signed(diff),
    };

    // Invariant: `*offset <= isize::MAX as usize`.
    // TODO: Investigate whether `read`/`write` can break this invariant.
    if new_offset.cast_signed() < 0 {
        return_errno_with_message!(Errno::EINVAL, "the file offset cannot be negative");
    }

    *offset = new_offset;
    Ok(new_offset)
}
