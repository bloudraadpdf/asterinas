// SPDX-License-Identifier: MPL-2.0

#![expect(dead_code)]

use super::Signal;
use crate::{
    prelude::*,
    process::{
        Pid, PidIdentity, PidNamespace, Uid,
        signal::{
            c_types::siginfo_t,
            constants::{SI_QUEUE, SI_TKILL, SI_USER, SIGKILL, SIGSTOP},
            sig_num::SigNum,
        },
    },
};

#[derive(Clone, Debug)]
pub(crate) struct UserSignal {
    num: SigNum,
    identity: Arc<PidIdentity>,
    uid: Uid,
    kind: UserSignalKind,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum UserSignalKind {
    Kill,
    Tkill,
    Sigqueue,
}

impl UserSignal {
    pub(crate) fn new(
        num: SigNum,
        kind: UserSignalKind,
        identity: Arc<PidIdentity>,
        uid: Uid,
    ) -> Self {
        Self {
            num,
            identity,
            uid,
            kind,
        }
    }

    pub(crate) fn new_kill(num: SigNum, ctx: &Context) -> Self {
        Self {
            num,
            identity: ctx.process.pid_identity().clone(),
            uid: ctx.posix_thread.credentials().ruid(),
            kind: UserSignalKind::Kill,
        }
    }

    pub(crate) fn pid(&self) -> Pid {
        self.identity.global()
    }

    pub(crate) fn kind(&self) -> UserSignalKind {
        self.kind
    }
}

impl Signal for UserSignal {
    fn num(&self) -> SigNum {
        self.num
    }

    fn to_info(&self, namespace: &PidNamespace) -> siginfo_t {
        let code = match self.kind {
            UserSignalKind::Kill => SI_USER,
            UserSignalKind::Tkill => SI_TKILL,
            UserSignalKind::Sigqueue => SI_QUEUE,
        };

        let mut info = siginfo_t::new(self.num, code);
        info.set_pid_uid(self.identity.in_namespace(namespace), self.uid);

        info
    }

    fn forces_namespace_init(&self, namespace: &Arc<PidNamespace>) -> bool {
        let sender_namespace = self.identity.namespace();
        (self.num == SIGKILL || self.num == SIGSTOP)
            && !Arc::ptr_eq(sender_namespace, namespace)
            && sender_namespace.is_same_or_ancestor_of(namespace)
    }
}
