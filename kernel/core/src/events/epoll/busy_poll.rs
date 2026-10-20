// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use keyable_arc::KeyableWeak;
use ostd::task::Task;

use crate::{
    dispatch_ioctl,
    fs::file::FileLike,
    prelude::*,
    process::{
        UserNamespace, credentials::capabilities::CapSet, posix_thread::AsPosixThread,
        signal::HandlePendingSignal,
    },
    security::lsm::hooks as lsm_hooks,
    time::clocks::MonotonicClock,
    util::ioctl::RawIoctl,
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct Params {
    busy_poll_usecs: u32,
    busy_poll_budget: u16,
    prefer_busy_poll: u8,
    pad: u8,
}

impl Params {
    fn enabled(self) -> bool {
        self.busy_poll_usecs != 0 || self.prefer_busy_poll != 0
    }

    fn validate(self) -> Result<()> {
        if self.pad != 0 || self.busy_poll_usecs > i32::MAX as u32 || self.prefer_busy_poll > 1 {
            return_errno!(Errno::EINVAL);
        }
        const UNPRIVILEGED_PACKET_BUDGET: u16 = 64;
        if self.busy_poll_budget > UNPRIVILEGED_PACKET_BUDGET {
            let current = current_thread!();
            lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
                UserNamespace::get_init_singleton().as_ref(),
                current.as_posix_thread().unwrap(),
                CapSet::NET_ADMIN,
            ))?;
        }
        Ok(())
    }
}

pub(super) struct BusyPoll {
    params: SpinLock<Params>,
    target: Mutex<Option<KeyableWeak<dyn FileLike>>>,
}

impl BusyPoll {
    pub(super) fn new() -> Self {
        Self {
            params: SpinLock::new(Params::default()),
            target: Mutex::new(None),
        }
    }

    pub(super) fn remember(&self, file: &KeyableWeak<dyn FileLike>) {
        if !self.params.lock().enabled() {
            return;
        }
        if let Some(strong) = file.upgrade()
            && strong.busy_poll(0).is_some()
        {
            *self.target.lock() = Some(file.clone());
        }
    }

    pub(super) fn run(&self, deadline: Option<Duration>, ready: impl Fn() -> bool) -> bool {
        let params = *self.params.lock();
        if !params.enabled() {
            return false;
        }
        let Some(target) = self.target.lock().clone() else {
            return false;
        };
        let Some(file) = target.upgrade() else {
            self.clear_target(&target);
            return false;
        };
        const DEFAULT_PACKET_BUDGET: u16 = 8;
        let budget = if params.busy_poll_budget == 0 {
            DEFAULT_PACKET_BUDGET
        } else {
            params.busy_poll_budget
        };
        let start = MonotonicClock::get().read_time();
        let end = start.saturating_add(Duration::from_micros(u64::from(params.busy_poll_usecs)));
        let current = current_thread!();
        loop {
            if deadline.is_some_and(|limit| MonotonicClock::get().read_time() >= limit)
                || current
                    .as_posix_thread()
                    .is_some_and(|thread| thread.has_pending())
            {
                break;
            }
            if file.busy_poll(budget).is_none() {
                break;
            }
            if ready() {
                return true;
            }
            if MonotonicClock::get().read_time() >= end {
                break;
            }
            // Timer softirqs may be deferred to softirqd when rescheduling is
            // requested. Let that task queue signals between bounded batches.
            if Task::need_yield() {
                Task::yield_now();
            } else {
                core::hint::spin_loop();
            }
        }
        self.clear_target(&target);
        false
    }

    fn clear_target(&self, target: &KeyableWeak<dyn FileLike>) {
        let mut current = self.target.lock();
        if current.as_ref().is_some_and(|file| file == target) {
            *current = None;
        }
    }

    pub(super) fn ioctl(&self, raw: RawIoctl) -> Result<i32> {
        use ioctl_defs::*;

        dispatch_ioctl!(match raw {
            command @ SetParams => {
                let params = command.read()?;
                params.validate()?;
                *self.params.lock() = params;
            }
            command @ GetParams => {
                let params = *self.params.lock();
                command.write(&params)?;
            }
            _ => return_errno!(Errno::EINVAL),
        });
        Ok(0)
    }
}

mod ioctl_defs {
    use super::Params;
    use crate::{
        ioc,
        util::ioctl::{InData, OutData},
    };

    pub(super) type SetParams = ioc!(EPIOCSPARAMS, 0x8a, 1, InData<Params>);
    pub(super) type GetParams = ioc!(EPIOCGPARAMS, 0x8a, 2, OutData<Params>);
}
