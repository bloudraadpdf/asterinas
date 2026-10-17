// SPDX-License-Identifier: MPL-2.0

use aster_rights::{Dup, Read, ReadDupOp, ReadOp, TRights};
use aster_rights_proc::require;

use crate::{
    prelude::*,
    process::{Credentials, Gid, Pid, PidIdentity, Uid, posix_thread::AsPosixThread},
};

pub(super) struct SocketCred<R = ReadOp> {
    identity: Arc<PidIdentity>,
    cred: Credentials<R>,
}

impl SocketCred<ReadOp> {
    pub(super) fn new_current() -> Self {
        let identity = current!().pid_identity().clone();
        let cred = current_thread!().as_posix_thread().unwrap().credentials();

        Self { identity, cred }
    }
}

impl SocketCred<ReadDupOp> {
    pub(super) fn new_current() -> Self {
        let identity = current!().pid_identity().clone();
        let cred = current_thread!()
            .as_posix_thread()
            .unwrap()
            .credentials_dup();

        Self { identity, cred }
    }
}

impl<R: TRights> SocketCred<R> {
    #[require(R > Read)]
    pub(super) fn same_real_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
            && self.cred.ruid() == other.cred.ruid()
            && self.cred.rgid() == other.cred.rgid()
    }

    /// Converts to a [`CUserCred`] with the PID and the _effective_ UID/GID.
    #[require(R > Read)]
    pub(super) fn to_effective_c_cred(&self) -> CUserCred {
        CUserCred {
            pid: self.identity.in_namespace(current!().pid_namespace()),
            uid: self.cred.euid(),
            gid: self.cred.egid(),
        }
    }

    /// Converts to a [`CUserCred`] with the PID and the _real_ UID/GID.
    #[require(R > Read)]
    pub(super) fn to_real_c_cred(&self) -> CUserCred {
        CUserCred {
            pid: self.identity.in_namespace(current!().pid_namespace()),
            uid: self.cred.ruid(),
            gid: self.cred.rgid(),
        }
    }

    #[require(R > Read)]
    pub(super) fn groups(&self) -> Arc<[Gid]> {
        self.cred.groups().iter().cloned().collect()
    }

    #[require(R > R1)]
    pub(super) fn restrict<R1: TRights>(self) -> SocketCred<R1> {
        let Self { identity, cred } = self;
        SocketCred {
            identity,
            cred: cred.restrict(),
        }
    }

    #[require(R > Dup)]
    pub(super) fn dup(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            cred: self.cred.dup(),
        }
    }
}

/// `struct ucred` in Linux.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.15/source/include/linux/socket.h#L183>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Pod)]
pub(crate) struct CUserCred {
    pid: Pid,
    uid: Uid,
    gid: Gid,
}

impl CUserCred {
    pub(in crate::net) const fn new_invalid() -> Self {
        Self {
            pid: 0,
            uid: Uid::INVALID,
            gid: Gid::INVALID,
        }
    }

    pub(in crate::net) const fn new_overflow() -> Self {
        Self {
            pid: 0,
            uid: Uid::OVERFLOW,
            gid: Gid::OVERFLOW,
        }
    }
}
