// SPDX-License-Identifier: MPL-2.0

use core::mem::offset_of;

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    fs::file::file_table::{FdFlags, RawFileDesc},
    net::socket::{Socket, util::RecvFlags},
    prelude::*,
    util::net::CUserMsgHdr,
};

pub(super) fn sys_recvmsg(
    sockfd: RawFileDesc,
    user_msghdr_ptr: Vaddr,
    flags: i32,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let file = super::socket::get_socket_file(sockfd, ctx)?;
    let socket = file.as_socket_or_err()?;
    let flags = RecvFlags::from_bits_truncate(flags);
    let (len, _) = recv_one_message(socket, user_msghdr_ptr, flags, ctx)?;
    Ok(SyscallReturn::Return(len as _))
}

pub(super) fn recv_one_message(
    socket: &dyn Socket,
    user_msghdr_ptr: Vaddr,
    flags: RecvFlags,
    ctx: &Context,
) -> Result<(usize, RecvFlags)> {
    let user_space = ctx.user_space();
    let header: CUserMsgHdr = user_space.read_val(user_msghdr_ptr)?;
    let mut writers = header.copy_writer_array_from_user(&user_space)?;
    let (output, message_header) = socket
        .recvmsg(&mut writers, flags)
        .map_err(|error| socket.restart_recv_error(error))?;

    let name_len = header.write_socket_addr_to_user(message_header.addr())?;
    if header.msg_name != 0 {
        user_space.write_val(
            user_msghdr_ptr + offset_of!(CUserMsgHdr, msg_namelen),
            &name_len,
        )?;
    }
    let fd_flags = if flags.contains(RecvFlags::MSG_CMSG_CLOEXEC) {
        FdFlags::CLOEXEC
    } else {
        FdFlags::empty()
    };
    let (control_len, control_flags) = header.write_control_messages_to_user(
        message_header.control_messages(),
        fd_flags,
        &user_space,
    )?;
    let output_flags = output.flags() | control_flags | (flags & RecvFlags::MSG_CMSG_CLOEXEC);
    user_space.write_val(
        user_msghdr_ptr + offset_of!(CUserMsgHdr, msg_flags),
        &output_flags.bits().cast_unsigned(),
    )?;
    user_space.write_val(
        user_msghdr_ptr + offset_of!(CUserMsgHdr, msg_controllen),
        &(control_len as usize),
    )?;
    Ok((output.len(), output_flags))
}
