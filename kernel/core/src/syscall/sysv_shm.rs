// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    ipc::{
        IpcFlags, IpcId,
        shared_memory::{SHM_REMAP, SHM_RND, ShmidDs},
    },
    prelude::*,
    vm::vmar::{VMAR_CAP_ADDR, VMAR_LOWEST_ADDR, VmarMapOffset},
};

fn id(value: i32) -> Result<IpcId> {
    IpcId::try_from(value.cast_unsigned()).map_err(|_| Error::new(Errno::EINVAL))
}

pub(super) fn sys_shmget(
    key: i32,
    size: usize,
    flags: i32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let flags = flags.cast_unsigned();
    if flags & 0o4000 != 0 {
        return_errno_with_message!(Errno::ENOSYS, "huge shared segments are not supported");
    }
    let ns = ctx.thread_local.borrow_ns_proxy();
    let id = ns.unwrap().ipc_ns().get_or_create_shared_segment(
        key,
        size,
        IpcFlags::from_bits_truncate(flags),
        (flags & 0o777) as u16,
        ctx,
    )?;
    Ok(SyscallReturn::Return(id.get() as isize))
}

pub(super) fn sys_shmat(
    segment: i32,
    address: Vaddr,
    flags: i32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let segment = id(segment)?;
    let flags = flags.cast_unsigned();
    let mut address = address;
    if !address.is_multiple_of(PAGE_SIZE) {
        if flags & SHM_RND == 0 {
            return_errno_with_message!(Errno::EINVAL, "the shared mapping address is not aligned");
        }
        address &= !(PAGE_SIZE - 1);
    }
    if flags & SHM_REMAP != 0 && address == 0 {
        return_errno_with_message!(Errno::EINVAL, "replacement requires a fixed address");
    }
    if address != 0 && address < VMAR_LOWEST_ADDR {
        return_errno_with_message!(Errno::EPERM, "the shared mapping address is too low");
    }
    let ns = ctx.thread_local.borrow_ns_proxy();
    let pending = ns.unwrap().ipc_ns().reserve_shm(segment, flags, ctx)?;
    if address != 0
        && VMAR_CAP_ADDR
            .checked_sub(address)
            .is_none_or(|gap| gap < pending.vmo().size())
    {
        return_errno_with_message!(
            Errno::EINVAL,
            "the shared mapping range is not in userspace"
        );
    }
    let offset = if address == 0 {
        VmarMapOffset::Any
    } else if flags & SHM_REMAP != 0 {
        VmarMapOffset::FixedReplace(address)
    } else {
        VmarMapOffset::FixedNoReplace(address)
    };
    let mapped = ctx
        .user_space()
        .vmar()
        .attach_shm(pending, offset)
        .map_err(|error| {
            if error.error() == Errno::EEXIST {
                Error::new(Errno::EINVAL)
            } else {
                error
            }
        })?;
    Ok(SyscallReturn::Return(mapped as isize))
}

pub(super) fn sys_shmdt(address: Vaddr, ctx: &Context) -> Result<SyscallReturn> {
    if !address.is_multiple_of(PAGE_SIZE) {
        return_errno_with_message!(Errno::EINVAL, "the shared mapping address is not aligned");
    }
    ctx.user_space().vmar().detach_shm(address)?;
    Ok(SyscallReturn::Return(0))
}

pub(super) fn sys_shmctl(
    segment: i32,
    command: i32,
    address: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let id = id(segment)?;
    let ns = ctx.thread_local.borrow_ns_proxy();
    let ns = ns.unwrap().ipc_ns();
    match command & !0x100 {
        0 => ns.remove_shared_segment(id, ctx)?,
        1 => {
            let input = ctx.user_space().read_val::<ShmidDs>(address)?;
            ns.set_shared_status(id, &input, ctx)?;
        }
        2 => {
            let status = ns.shared_status(id, ctx)?;
            ctx.user_space().write_val(address, &status)?;
        }
        _ => return_errno_with_message!(Errno::EINVAL, "the shared segment command is invalid"),
    }
    Ok(SyscallReturn::Return(0))
}
