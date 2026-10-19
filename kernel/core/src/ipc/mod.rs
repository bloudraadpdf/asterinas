// SPDX-License-Identifier: MPL-2.0

use crate::{
    prelude::*,
    process::{Gid, Uid, UserNamespace, credentials::capabilities::CapSet},
};

mod ipc_ids;
mod ipc_ns;
pub(crate) mod message;
pub(crate) mod semaphore;
pub(crate) mod shared_memory;

pub(crate) use ipc_ids::IpcId;
pub(crate) use ipc_ns::IpcNamespace;

/// An IPC key.
///
/// This key is specified by and can be looked up in userspace. Do not confuse it with the
/// identifier [`IpcId`], which is specified by the kernel and returned to userspace upon lookup.
pub(crate) type IpcKey = i32;

/// A private IPC key.
///
/// This key cannot be looked up.
const IPC_PRIVATE: IpcKey = 0;

bitflags! {
    pub(crate) struct IpcAccess: u16 {
        const READ = 0o004;
        const WRITE = 0o002;
        const EXECUTE = 0o001;
    }
}

impl IpcAccess {
    fn from_mode(mode: u16) -> Self {
        Self::from_bits_truncate(mode | (mode >> 3) | (mode >> 6))
    }
}

#[padding_struct]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct IpcPerm {
    key: u32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u16,
    _pad1: u16,
    seq: u16,
    _pad2: u16,
    _unused1: u64,
    _unused2: u64,
}

bitflags! {
    pub(crate) struct IpcFlags: u32{
        /// Create key if key does not exist
        const IPC_CREAT  = 1 << 9;
        /// Fail if key exists
        const IPC_EXCL  = 1 << 10;
        /// Return error on wait
        const IPC_NOWAIT  = 1 << 11;
        /// Undo the operation on exit
        const SEM_UNDO = 1 << 12;
    }
}

#[expect(non_camel_case_types)]
#[repr(i32)]
#[derive(Clone, Copy, Debug, TryFromInt)]
pub(crate) enum IpcControlCmd {
    IPC_RMID = 0,
    IPC_SET = 1,
    IPC_STAT = 2,

    SEM_GETPID = 11,
    SEM_GETVAL = 12,
    SEM_GETALL = 13,
    SEM_GETNCNT = 14,
    SEM_GETZCNT = 15,
    SEM_SETVAL = 16,
    SEM_SETALL = 17,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IpcPermission {
    key: IpcKey,
    /// Owner's UID
    uid: Uid,
    /// Owner's GID
    gid: Gid,
    /// Creator's UID
    cuid: Uid,
    /// Creator's GID
    cguid: Gid,
    /// Permission mode
    mode: u16,
}

impl IpcPermission {
    pub(crate) fn key(&self) -> IpcKey {
        self.key
    }

    /// Returns owner's UID
    pub(crate) fn uid(&self) -> Uid {
        self.uid
    }

    /// Returns owner's GID
    pub(crate) fn gid(&self) -> Gid {
        self.gid
    }

    /// Returns creator's UID
    pub(crate) fn cuid(&self) -> Uid {
        self.cuid
    }

    /// Returns creator's GID
    pub(crate) fn cguid(&self) -> Gid {
        self.cguid
    }

    /// Returns permission mode
    pub(crate) fn mode(&self) -> u16 {
        self.mode
    }

    pub(in crate::ipc) fn check_access(
        &self,
        required: IpcAccess,
        ctx: &Context,
        owner: &UserNamespace,
    ) -> Result<()> {
        let credentials = ctx.posix_thread.credentials();
        let euid = credentials.euid();
        let granted = if euid == self.uid() || euid == self.cuid() {
            self.mode() >> 6
        } else if credentials.egid() == self.gid()
            || credentials.egid() == self.cguid()
            || credentials
                .groups()
                .iter()
                .any(|gid| *gid == self.gid() || *gid == self.cguid())
        {
            self.mode() >> 3
        } else {
            self.mode()
        };
        if required.bits() & !granted != 0
            && owner
                .check_capability(ctx.posix_thread, CapSet::IPC_OWNER)
                .is_err()
        {
            return_errno_with_message!(Errno::EACCES, "IPC access is denied");
        }
        Ok(())
    }

    pub(in crate::ipc) fn check_owner(&self, ctx: &Context, owner: &UserNamespace) -> Result<()> {
        let euid = ctx.posix_thread.credentials().euid();
        if euid != self.uid()
            && euid != self.cuid()
            && owner
                .check_capability(ctx.posix_thread, CapSet::SYS_ADMIN)
                .is_err()
        {
            return_errno_with_message!(Errno::EPERM, "the caller does not own the IPC object");
        }
        Ok(())
    }

    fn update(&mut self, input: &IpcPerm) -> Result<()> {
        if input.uid == u32::MAX || input.gid == u32::MAX {
            return_errno_with_message!(Errno::EINVAL, "the IPC owner identity is invalid");
        }
        self.uid = Uid::new(input.uid);
        self.gid = Gid::new(input.gid);
        self.mode = (self.mode & !0o777) | (input.mode & 0o777);
        Ok(())
    }

    fn abi(&self) -> IpcPerm {
        IpcPerm {
            key: self.key().cast_unsigned(),
            uid: self.uid().into(),
            gid: self.gid().into(),
            cuid: self.cuid().into(),
            cgid: self.cguid().into(),
            mode: self.mode(),
            ..IpcPerm::default()
        }
    }

    pub(self) fn new(key: IpcKey, uid: Uid, gid: Gid, mode: u16) -> Self {
        Self {
            key,
            uid,
            gid,
            cuid: uid,
            cguid: gid,
            mode,
        }
    }
}
