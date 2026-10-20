// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    fs::{
        self,
        file::{
            FileLike, InodeHandle, InodeType, StatusFlags,
            file_table::{RawFileDesc, WithFileTable},
        },
    },
    prelude::*,
    process::{
        ResourceType,
        signal::{constants::SIGXFSZ, signals::kernel::KernelSignal},
    },
};

pub(super) fn sys_copy_file_range(
    in_fd: RawFileDesc,
    in_offset_ptr: Vaddr,
    out_fd: RawFileDesc,
    out_offset_ptr: Vaddr,
    length: usize,
    flags: u32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let (input, output) = ctx
        .thread_local
        .borrow_file_table_mut()
        .read_with(|table| {
            Ok::<_, Error>((
                table.get_file(in_fd.try_into()?)?.clone(),
                table.get_file(out_fd.try_into()?)?.clone(),
            ))
        })?;
    let mut positions =
        CopyPositions::lock(&input, in_offset_ptr == 0, &output, out_offset_ptr == 0);
    let input_offset = if in_offset_ptr == 0 {
        positions.input_offset()
    } else {
        ctx.user_space().read_val(in_offset_ptr)?
    };
    let output_offset = if out_offset_ptr == 0 {
        positions.output_offset()
    } else {
        ctx.user_space().read_val(out_offset_ptr)?
    };
    if flags != 0 {
        return_errno_with_message!(Errno::EINVAL, "copy flags must be zero");
    }

    let count = check_copy(&input, input_offset, &output, output_offset, length, ctx)?;
    // Like Linux's same-filesystem fallback, copying need not clone filesystem extents.
    // Reference: <https://github.com/torvalds/linux/blob/v7.0/fs/read_write.c#L1390-L1604>.
    let copied = copy_bytes(
        &input,
        input_offset as usize,
        &output,
        output_offset as usize,
        count,
    )?;
    if copied == 0 {
        return Ok(SyscallReturn::Return(0));
    }

    fs::vfs::notify::on_access(&input);
    fs::vfs::notify::on_modify(&output);
    // Offset copyout may fault after data has been copied. Both positions must still be updated.
    let input_result = if in_offset_ptr != 0 {
        ctx.user_space()
            .write_val(in_offset_ptr, &(input_offset + copied as i64))
            .map_err(Error::from)
    } else {
        positions.set_input(input_offset as usize + copied);
        Ok(())
    };
    let output_result = if out_offset_ptr != 0 {
        ctx.user_space()
            .write_val(out_offset_ptr, &(output_offset + copied as i64))
            .map_err(Error::from)
    } else {
        positions.set_output(output_offset as usize + copied);
        Ok(())
    };
    input_result?;
    output_result?;
    Ok(SyscallReturn::Return(copied as _))
}

struct CopyPositions<'a> {
    input: Option<MutexGuard<'a, usize>>,
    output: Option<MutexGuard<'a, usize>>,
    shared: bool,
}

impl<'a> CopyPositions<'a> {
    fn lock(
        input: &'a Arc<dyn FileLike>,
        lock_input: bool,
        output: &'a Arc<dyn FileLike>,
        lock_output: bool,
    ) -> Self {
        let input_handle = lock_input
            .then(|| input.downcast_ref::<InodeHandle>())
            .flatten();
        let output_handle = lock_output
            .then(|| output.downcast_ref::<InodeHandle>())
            .flatten();
        let shared =
            input_handle.is_some() && output_handle.is_some() && Arc::ptr_eq(input, output);
        let (input, output) = if shared {
            (input_handle.map(InodeHandle::lock_offset), None)
        } else if Arc::as_ptr(input).cast::<()>() < Arc::as_ptr(output).cast::<()>() {
            let input = input_handle.map(InodeHandle::lock_offset);
            let output = output_handle.map(InodeHandle::lock_offset);
            (input, output)
        } else {
            let output = output_handle.map(InodeHandle::lock_offset);
            let input = input_handle.map(InodeHandle::lock_offset);
            (input, output)
        };
        Self {
            input,
            output,
            shared,
        }
    }

    fn input_offset(&self) -> i64 {
        self.input.as_deref().copied().unwrap_or(0) as i64
    }

    fn output_offset(&self) -> i64 {
        if self.shared {
            self.input_offset()
        } else {
            self.output.as_deref().copied().unwrap_or(0) as i64
        }
    }

    fn set_input(&mut self, offset: usize) {
        if let Some(position) = self.input.as_deref_mut() {
            *position = offset;
        }
    }

    fn set_output(&mut self, offset: usize) {
        if self.shared {
            self.set_input(offset);
        } else if let Some(position) = self.output.as_deref_mut() {
            *position = offset;
        }
    }
}

fn check_copy(
    input: &Arc<dyn FileLike>,
    input_offset: i64,
    output: &Arc<dyn FileLike>,
    output_offset: i64,
    length: usize,
    ctx: &Context,
) -> Result<usize> {
    let input_path = input.path();
    let output_path = output.path();
    if input_path.type_() == InodeType::Dir || output_path.type_() == InodeType::Dir {
        return_errno_with_message!(Errno::EISDIR, "copying directories is not supported");
    }
    if !input_path.type_().is_regular_file() || !output_path.type_().is_regular_file() {
        return_errno_with_message!(Errno::EINVAL, "copying requires regular files");
    }
    if input.status_flags().contains(StatusFlags::O_PATH)
        || output
            .status_flags()
            .intersects(StatusFlags::O_PATH | StatusFlags::O_APPEND)
        || !input.access_mode().is_readable()
        || !output.access_mode().is_writable()
    {
        return_errno_with_message!(Errno::EBADF, "the file access modes do not permit copying");
    }
    if !Arc::ptr_eq(&input_path.fs(), &output_path.fs()) {
        return_errno_with_message!(Errno::EXDEV, "copying across filesystems is not supported");
    }
    if (input_offset as u64).checked_add(length as u64).is_none()
        || (output_offset as u64).checked_add(length as u64).is_none()
    {
        return_errno_with_message!(Errno::EOVERFLOW, "the copy range overflows");
    }
    if input_offset < 0 || output_offset < 0 {
        return_errno_with_message!(Errno::EINVAL, "copy offsets cannot be negative");
    }
    let input_offset = input_offset as usize;
    let output_offset = output_offset as usize;
    let mut count = length.min(input_path.size().saturating_sub(input_offset));
    let file_size_limit = ctx
        .process
        .resource_limits()
        .get_rlimit(ResourceType::RLIMIT_FSIZE)
        .get_cur();
    if output_offset as u64 >= file_size_limit {
        ctx.posix_thread
            .enqueue_signal(Box::new(KernelSignal::new(SIGXFSZ)));
        return_errno_with_message!(Errno::EFBIG, "the copy starts beyond the file size limit");
    }
    if output_offset >= i64::MAX as usize {
        return_errno_with_message!(
            Errno::EFBIG,
            "the output offset exceeds the maximum file size"
        );
    }
    count = count.min((file_size_limit - output_offset as u64) as usize);
    count = count.min(i64::MAX as usize - output_offset);
    if Arc::ptr_eq(input_path.inode(), output_path.inode())
        && output_offset + count > input_offset
        && output_offset < input_offset + count
    {
        return_errno_with_message!(Errno::EINVAL, "copy ranges in the same file overlap");
    }
    if count > i64::MAX as usize || input_offset > i64::MAX as usize - count {
        return_errno_with_message!(Errno::EINVAL, "the input range exceeds signed file offsets");
    }
    // Linux's generic copy fallback uses MAX_RW_COUNT, even for a larger request.
    const MAX_RW_COUNT: usize = 0x7fff_f000;
    Ok(count.min(MAX_RW_COUNT))
}

fn copy_bytes(
    input: &Arc<dyn FileLike>,
    input_offset: usize,
    output: &Arc<dyn FileLike>,
    output_offset: usize,
    count: usize,
) -> Result<usize> {
    let mut buffer = [0u8; PAGE_SIZE];
    let mut copied = 0;
    while copied < count {
        let chunk = buffer.len().min(count - copied);
        let read = match input.read_bytes_at(input_offset + copied, &mut buffer[..chunk]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) if copied != 0 => break,
            Err(error) => return Err(error),
        };
        let written = match output.write_bytes_at(output_offset + copied, &buffer[..read]) {
            Ok(written) => written,
            Err(_) if copied != 0 => break,
            Err(error) => return Err(error),
        };
        copied += written;
        if written < read {
            break;
        }
    }
    Ok(copied)
}
