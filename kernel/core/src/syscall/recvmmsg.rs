// SPDX-License-Identifier: MPL-2.0

use core::{mem::offset_of, time::Duration};

use aster_time::read_monotonic_time;
use ostd::mm::VmIo;

use super::{SyscallReturn, recvmsg::recv_one_message};
use crate::{
    fs::file::file_table::RawFileDesc,
    net::socket::{options::Error as SocketError, util::RecvFlags},
    prelude::*,
    time::timespec_t,
    util::net::CUserMMsgHdr,
};

pub(super) fn sys_recvmmsg(
    sockfd: RawFileDesc,
    messages_addr: Vaddr,
    count: u32,
    flags: i32,
    timeout_addr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    const MSG_CMSG_COMPAT: i32 = i32::MIN;
    if flags & MSG_CMSG_COMPAT != 0 {
        return_errno!(Errno::EINVAL);
    }
    let user_space = ctx.user_space();
    let mut remaining = if timeout_addr != 0 {
        Some(Duration::try_from(
            user_space.read_val::<timespec_t>(timeout_addr)?,
        )?)
    } else {
        None
    };
    let deadline = remaining.map(|duration| read_monotonic_time().saturating_add(duration));
    let file = super::socket::get_socket_file(sockfd, ctx)?;
    let socket = file.as_socket_or_err()?;
    let mut flags = RecvFlags::from_bits_truncate(flags);
    if !flags.contains(RecvFlags::MSG_ERRQUEUE) {
        let mut option = SocketError::new();
        socket.get_option(&mut option)?;
        if let Some(error) = option.get().copied().flatten() {
            return Err(error);
        }
    }

    let mut received = 0u32;
    while received < count {
        let result: Result<RecvFlags> = (|| {
            let addr = messages_addr
                .checked_add(received as usize * size_of::<CUserMMsgHdr>())
                .ok_or_else(|| Error::new(Errno::EFAULT))?;
            let (len, output_flags) =
                recv_one_message(socket, addr, flags & !RecvFlags::MSG_WAITFORONE, ctx)?;
            user_space.write_val(addr + offset_of!(CUserMMsgHdr, msg_len), &(len as u32))?;
            Ok(output_flags)
        })();
        let output_flags = match result {
            Ok(output_flags) => output_flags,
            Err(error) if received == 0 => return Err(error),
            Err(error) => {
                if error.error() != Errno::EAGAIN {
                    socket.pending_error().set(error);
                }
                break;
            }
        };
        received += 1;
        if flags.contains(RecvFlags::MSG_WAITFORONE) {
            flags |= RecvFlags::MSG_DONTWAIT;
        }
        if let Some(deadline) = deadline {
            let left = deadline.saturating_sub(read_monotonic_time());
            remaining = Some(left);
            if left.is_zero() {
                break;
            }
        }
        if output_flags.contains(RecvFlags::MSG_OOB) {
            break;
        }
    }
    if received != 0
        && let Some(remaining) = remaining
    {
        user_space.write_val(timeout_addr, &timespec_t::from(remaining))?;
    }
    Ok(SyscallReturn::Return(received as _))
}
