// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::{SyscallReturn, open};
use crate::{
    fs::{
        file::{
            AccessMode, CreationFlags, InodeMode, OpenArgs, StatusFlags,
            file_table::{FdFlags, FileTable, RawFileDesc},
        },
        utils::PATH_MAX,
        vfs::path::{LookupResult, ResolveFlags},
    },
    prelude::*,
};

const OPEN_HOW_SIZE: usize = 3 * size_of::<u64>();
const O_LARGEFILE: u64 = 1 << 15;

struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

impl OpenHow {
    fn read(address: Vaddr, size: usize, ctx: &Context) -> Result<Self> {
        if size < OPEN_HOW_SIZE {
            return_errno_with_message!(Errno::EINVAL, "the open_how structure is too small");
        }
        if size > PAGE_SIZE {
            return_errno_with_message!(Errno::E2BIG, "the open_how structure is too large");
        }
        let user_space = ctx.user_space();
        let [flags, mode, resolve] = user_space.read_val::<[u64; 3]>(address)?;
        let mut extension = [0; 128];
        let mut offset = OPEN_HOW_SIZE;
        while offset < size {
            let length = extension.len().min(size - offset);
            let start = address
                .checked_add(offset)
                .ok_or_else(|| Error::new(Errno::EFAULT))?;
            user_space.read_bytes(start, &mut extension[..length])?;
            if extension[..length].iter().any(|byte| *byte != 0) {
                return_errno_with_message!(Errno::E2BIG, "the open_how extension is not zero");
            }
            offset += length;
        }
        Ok(Self {
            flags,
            mode,
            resolve,
        })
    }

    fn validate(&self) -> Result<(u32, u16, ResolveFlags)> {
        let known_flags = u64::from(CreationFlags::all().bits() | StatusFlags::all().bits())
            | u64::from(AccessMode::MASK)
            | O_LARGEFILE;
        if self.flags & !known_flags != 0 {
            return_errno_with_message!(Errno::EINVAL, "unknown open flags");
        }
        let resolve = ResolveFlags::from_bits(self.resolve)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "unknown path resolution flags"))?;
        if resolve.contains(ResolveFlags::BENEATH | ResolveFlags::IN_ROOT) {
            return_errno_with_message!(Errno::EINVAL, "BENEATH and IN_ROOT are mutually exclusive");
        }
        let mut flags = self.flags as u32;
        if flags & StatusFlags::O_PATH.bits() != 0 {
            let allowed = StatusFlags::O_PATH.bits()
                | CreationFlags::O_DIRECTORY.bits()
                | CreationFlags::O_NOFOLLOW.bits()
                | CreationFlags::O_CLOEXEC.bits();
            if flags & !allowed != 0 {
                return_errno_with_message!(Errno::EINVAL, "invalid flags with O_PATH");
            }
        }
        let creates = flags & (CreationFlags::O_CREAT | CreationFlags::O_TMPFILE).bits() != 0;
        if self.mode & !0o7777 != 0 || !creates && self.mode != 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid file creation mode");
        }
        if flags & StatusFlags::O_SYNC.bits() != 0 {
            flags |= StatusFlags::O_DSYNC.bits();
        }
        OpenArgs::from_flags_and_mode(flags, InodeMode::from_bits_truncate(self.mode as u16))?;
        if resolve.contains(ResolveFlags::CACHED)
            && flags
                & (CreationFlags::O_CREAT | CreationFlags::O_TRUNC | CreationFlags::O_TMPFILE)
                    .bits()
                != 0
        {
            return_errno_with_message!(
                Errno::EAGAIN,
                "file creation or truncation requires an uncached open"
            );
        }
        Ok((flags, self.mode as u16, resolve))
    }
}

pub(super) fn sys_openat2(
    dirfd: RawFileDesc,
    path_address: Vaddr,
    how_address: Vaddr,
    size: usize,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let how = OpenHow::read(how_address, size, ctx)?;
    let (flags, mode, resolve) = how.validate()?;
    let name = ctx.user_space().read_cstring(path_address, PATH_MAX);
    let descriptor = FileTable::reserve(
        ctx.thread_local.borrow_file_table().unwrap(),
        ctx.process.resource_limits().nofile(),
    )?;
    let name = name?;
    if name.as_bytes().is_empty() {
        return_errno_with_message!(Errno::ENOENT, "the path is empty");
    }
    let file_handle = {
        let name = name.to_string_lossy();
        let fs = ctx.thread_local.borrow_fs();
        let resolver = if resolve.contains(ResolveFlags::CACHED) {
            fs.resolver().try_read().ok_or_else(|| {
                Error::with_message(Errno::EAGAIN, "the process path resolver is busy")
            })?
        } else {
            fs.resolver().read()
        };
        let mode = InodeMode::from_bits_truncate(mode & !fs.umask().get());
        if resolve.is_empty() {
            let path = crate::fs::vfs::path::FsPath::from_fd_at(
                dirfd,
                &name,
                crate::fs::vfs::path::EmptyPathStr::Reject,
            )?;
            open::do_open(&resolver, &path, flags, mode)
        } else {
            let arguments = OpenArgs::from_flags_and_mode(flags, mode)?;
            let result =
                resolver.lookup_openat2(dirfd, &name, arguments.follow_tail_link(), resolve)?;
            if resolve.contains(ResolveFlags::CACHED)
                && !arguments.status_flags.contains(StatusFlags::O_PATH)
                && let LookupResult::Resolved(path) = &result
            {
                path.inode()
                    .check_permission_cached(arguments.access_mode.into())?;
            }
            if arguments.is_tmpfile() {
                let LookupResult::Resolved(directory) = result else {
                    return_errno_with_message!(
                        Errno::ENOENT,
                        "the temporary file directory does not exist"
                    );
                };
                open::open_tmpfile_at(directory, &arguments)
            } else {
                open::open_lookup(result, arguments)
            }
        }
    }
    .map_err(|error| match error.error() {
        Errno::EINTR => Error::new(Errno::ERESTARTSYS),
        _ => error,
    })?;
    let fd_flags = if flags & CreationFlags::O_CLOEXEC.bits() != 0 {
        FdFlags::CLOEXEC
    } else {
        FdFlags::empty()
    };
    let fd = descriptor.install(file_handle.clone(), fd_flags);
    crate::fs::vfs::notify::on_open(file_handle.common());
    Ok(SyscallReturn::Return(fd.into()))
}
