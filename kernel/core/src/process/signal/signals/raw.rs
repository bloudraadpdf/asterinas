// SPDX-License-Identifier: MPL-2.0

use alloc::sync::Arc;
use core::fmt::Debug;

use crate::process::{
    PidIdentity, PidNamespace,
    signal::{c_types::siginfo_t, sig_num::SigNum, signals::Signal},
};

/// A signal that carries raw [`siginfo_t`] information.
#[derive(Clone)]
pub(crate) struct RawSignal {
    info: siginfo_t,
    origin: Origin,
}

#[derive(Clone)]
enum Origin {
    Process(Arc<PidIdentity>),
    User(Arc<PidNamespace>),
}

impl Debug for RawSignal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawSignal")
            .field("signo", &self.info.si_signo)
            .field("errno", &self.info.si_errno)
            .field("code", &self.info.si_code)
            .finish_non_exhaustive()
    }
}

impl RawSignal {
    /// Creates a signal that carries raw [`siginfo_t`] information.
    ///
    /// The caller must ensure that the `info.si_signo` is a valid signal number.
    pub(crate) fn from_user(info: siginfo_t, origin: Arc<PidNamespace>) -> Self {
        Self {
            info,
            origin: Origin::User(origin),
        }
    }

    pub(crate) fn discard_info(&mut self) {
        self.info = siginfo_t::new(self.num(), super::super::constants::SI_USER);
    }

    pub(crate) fn from_process(info: siginfo_t, sender: Arc<PidIdentity>) -> Self {
        Self {
            info,
            origin: Origin::Process(sender),
        }
    }
}

impl Signal for RawSignal {
    fn num(&self) -> SigNum {
        SigNum::from_u8(self.info.si_signo as u8)
    }

    fn to_info(&self, namespace: &PidNamespace) -> siginfo_t {
        let mut info = self.info;
        if let Origin::Process(sender) = &self.origin {
            let (_, uid) = info.pid_uid();
            info.set_pid_uid(sender.in_namespace(namespace), uid);
        }
        info
    }

    fn forces_namespace_init(&self, namespace: &Arc<PidNamespace>) -> bool {
        use crate::process::signal::constants::{SIGKILL, SIGSTOP};
        let origin = match &self.origin {
            Origin::Process(sender) => sender.namespace(),
            Origin::User(origin) => origin,
        };
        (self.num() == SIGKILL || self.num() == SIGSTOP)
            && !Arc::ptr_eq(origin, namespace)
            && origin.is_same_or_ancestor_of(namespace)
    }
}
