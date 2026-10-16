// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    prelude::*,
    process::{UserNamespace, credentials::capabilities::CapSet},
    security::lsm::hooks,
    time::{SystemTime, clockid_t, timespec_t},
};

pub(super) fn sys_clock_settime(
    clockid: clockid_t,
    address: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if clockid != super::ClockId::CLOCK_REALTIME as clockid_t {
        return_errno_with_message!(Errno::EINVAL, "the clock cannot be set");
    }
    let value: timespec_t = ctx.user_space().read_val(address)?;
    let duration = Duration::try_from(value)?;
    hooks::on_capable(hooks::CapableContext::new(
        UserNamespace::get_init_singleton().as_ref(),
        ctx.posix_thread,
        CapSet::SYS_TIME,
    ))?;
    SystemTime::set_realtime(duration)?;
    Ok(SyscallReturn::Return(0))
}
