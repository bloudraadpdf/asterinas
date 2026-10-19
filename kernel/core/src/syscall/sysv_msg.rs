// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    ipc::{
        IpcFlags, IpcId,
        message::{Message, MsgidDs},
    },
    prelude::*,
};

fn id(value: i32) -> Result<IpcId> {
    IpcId::try_from(value.cast_unsigned()).map_err(|_| Error::new(Errno::EINVAL))
}

pub(super) fn sys_msgget(key: i32, flags: i32, ctx: &Context) -> Result<SyscallReturn> {
    let ns = ctx.thread_local.borrow_ns_proxy();
    let flags = flags.cast_unsigned();
    let id = ns.unwrap().ipc_ns().get_or_create_message_queue(
        key,
        IpcFlags::from_bits_truncate(flags),
        (flags & 0o777) as u16,
        ctx,
    )?;
    Ok(SyscallReturn::Return(id.get() as isize))
}

pub(super) fn sys_msgsnd(
    queue: i32,
    address: Vaddr,
    size: usize,
    flags: i32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if queue < 0 {
        return_errno_with_message!(Errno::EINVAL, "the message queue ID is negative");
    }
    let message = Message::from_user(address, size, ctx)?;
    let ns = ctx.thread_local.borrow_ns_proxy();
    ns.unwrap()
        .ipc_ns()
        .message_queue(id(queue)?)?
        .send(message, flags.cast_unsigned(), ctx)?;
    Ok(SyscallReturn::Return(0))
}

pub(super) fn sys_msgrcv(
    queue: i32,
    address: Vaddr,
    size: usize,
    wanted: isize,
    flags: i32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if queue < 0 || size > isize::MAX as usize {
        return_errno_with_message!(
            Errno::EINVAL,
            "the message queue ID or receive size is invalid"
        );
    }
    let flags = flags.cast_unsigned();
    if flags & 0o40000 != 0 {
        if flags & 0o20000 != 0
            || !IpcFlags::from_bits_truncate(flags).contains(IpcFlags::IPC_NOWAIT)
        {
            return_errno_with_message!(Errno::EINVAL, "invalid MSG_COPY flags");
        }
        return_errno_with_message!(Errno::ENOSYS, "checkpoint message copies are not supported");
    }
    let ns = ctx.thread_local.borrow_ns_proxy();
    let message =
        ns.unwrap()
            .ipc_ns()
            .message_queue(id(queue)?)?
            .receive(wanted as i64, size, flags, ctx)?;
    let count = message.copy_to_user(address, size, ctx)?;
    Ok(SyscallReturn::Return(count as isize))
}

pub(super) fn sys_msgctl(
    queue: i32,
    command: i32,
    address: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let id = id(queue)?;
    let ns = ctx.thread_local.borrow_ns_proxy();
    let ns = ns.unwrap().ipc_ns();
    match command & !0x100 {
        0 => ns.remove_message_queue(id, ctx)?,
        1 => {
            let input = ctx.user_space().read_val::<MsgidDs>(address)?;
            ns.message_queue(id)?.set_status(&input, ctx)?;
        }
        2 => {
            let status = ns.message_queue(id)?.status(ctx)?;
            ctx.user_space().write_val(address, &status)?;
        }
        _ => return_errno_with_message!(Errno::EINVAL, "the message queue command is invalid"),
    }
    Ok(SyscallReturn::Return(0))
}
