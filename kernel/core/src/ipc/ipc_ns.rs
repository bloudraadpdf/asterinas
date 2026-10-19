// SPDX-License-Identifier: MPL-2.0

//! Defines the IPC namespace abstraction.
//!
//! An IPC namespace isolates System V IPC resources from other namespaces.
//! Semaphore sets, message queues and shared segments share key, ID and
//! permission handling.

use spin::Once;

use super::{
    IpcAccess, IpcFlags, IpcId, IpcKey, IpcPerm,
    ipc_ids::{IpcIds, IpcRemoval},
    message::{MSGMNI, MessageQueue},
    semaphore::system_v::sem_set::{SEMMNI, SemaphoreSet},
    shared_memory::{PendingAttachment, SHMALL, SHMMAX, SHMMNI, SharedSegment, ShmidDs},
};
use crate::{
    fs::pseudofs::{NsCommonOps, NsType, StashedDentry},
    prelude::*,
    process::{UserNamespace, credentials::capabilities::CapSet, posix_thread::PosixThread},
    util::quota::Quota,
};

/// The IPC namespace.
///
/// An IPC namespace isolates System V IPC objects
/// (semaphores, message queues, shared memory).
/// Each namespace maintains its own independent set
/// of IPC resources and identifier allocator.
///
/// Lock ordering:
/// ID tables -> object state -> semaphore undo state.
pub(crate) struct IpcNamespace {
    /// Semaphore sets within this namespace.
    sem_ids: IpcIds<Arc<SemaphoreSet>>,
    message_ids: IpcIds<Arc<MessageQueue>>,
    shared_ids: IpcIds<Arc<SharedSegment>>,
    shared_pages: Arc<Quota>,
    /// Owner user namespace.
    owner: Arc<UserNamespace>,
    /// Stashed dentry for nsfs.
    stashed_dentry: StashedDentry,
}

impl IpcNamespace {
    /// Returns a reference to the singleton initial IPC namespace.
    pub(crate) fn get_init_singleton() -> &'static Arc<IpcNamespace> {
        static INIT: Once<Arc<IpcNamespace>> = Once::new();

        INIT.call_once(|| {
            let owner = UserNamespace::get_init_singleton().clone();
            Self::new(owner)
        })
    }

    fn new(owner: Arc<UserNamespace>) -> Arc<Self> {
        let sem_ids = IpcIds::new(SEMMNI);
        let stashed_dentry = StashedDentry::new();

        Arc::new(Self {
            sem_ids,
            message_ids: IpcIds::new(MSGMNI),
            shared_ids: IpcIds::new(SHMMNI),
            shared_pages: Quota::new(SHMALL),
            owner,
            stashed_dentry,
        })
    }

    /// Clones a new IPC namespace from `self`.
    ///
    /// The new namespace starts with an empty set of IPC resources.
    pub(crate) fn new_clone(
        &self,
        owner: Arc<UserNamespace>,
        posix_thread: &PosixThread,
    ) -> Result<Arc<Self>> {
        owner.check_capability(posix_thread, CapSet::SYS_ADMIN)?;
        Ok(Self::new(owner))
    }

    pub(crate) fn get_or_create_shared_segment(
        &self,
        key: IpcKey,
        size: usize,
        flags: IpcFlags,
        mode: u16,
        ctx: &Context,
    ) -> Result<IpcId> {
        let validate = if size == 0 || size > SHMMAX {
            Err(Error::new(Errno::EINVAL))
        } else {
            Ok(())
        };
        self.shared_ids.get_or_create(
            key,
            flags,
            validate,
            |id| {
                Ok(Arc::new(SharedSegment::new(
                    id,
                    key,
                    size,
                    mode,
                    &self.shared_pages,
                    ctx,
                )?))
            },
            |segment| segment.check_lookup(size, mode, ctx, &self.owner),
        )
    }

    pub(crate) fn reserve_shm(
        self: &Arc<Self>,
        id: IpcId,
        flags: u32,
        ctx: &Context,
    ) -> Result<PendingAttachment> {
        self.shared_ids
            .with(id, |segment| segment.reserve(self, flags, ctx, &self.owner))?
    }

    pub(crate) fn shared_status(&self, id: IpcId, ctx: &Context) -> Result<ShmidDs> {
        self.shared_ids
            .with(id, |segment| segment.status(ctx, &self.owner))?
    }

    pub(crate) fn set_shared_status(
        &self,
        id: IpcId,
        input: &ShmidDs,
        ctx: &Context,
    ) -> Result<()> {
        self.shared_ids
            .update(id, |segment| segment.set_status(input, ctx, &self.owner))?
    }

    pub(crate) fn remove_shared_segment(&self, id: IpcId, ctx: &Context) -> Result<()> {
        self.shared_ids.remove(id, |segment| {
            Ok(if segment.mark_removed(ctx, &self.owner)? {
                IpcRemoval::Destroy
            } else {
                IpcRemoval::Unname
            })
        })
    }

    pub(super) fn finalize_shm(&self, id: IpcId, expected: &Arc<SharedSegment>) {
        let _ = self.shared_ids.remove(id, |segment| {
            Ok(
                if Arc::ptr_eq(segment, expected) && segment.ready_to_destroy() {
                    IpcRemoval::Destroy
                } else {
                    IpcRemoval::Keep
                },
            )
        });
    }

    pub(crate) fn get_or_create_message_queue(
        &self,
        key: IpcKey,
        flags: IpcFlags,
        mode: u16,
        ctx: &Context,
    ) -> Result<IpcId> {
        self.message_ids.get_or_create(
            key,
            flags,
            Ok(()),
            |_| {
                Ok(Arc::new(MessageQueue::new(
                    key,
                    mode,
                    self.owner.clone(),
                    ctx,
                )))
            },
            |queue| queue.check_lookup(mode, ctx),
        )
    }

    pub(crate) fn message_queue(&self, id: IpcId) -> Result<Arc<MessageQueue>> {
        Ok(self.message_ids.with(id, Arc::clone)?)
    }

    pub(crate) fn remove_message_queue(&self, id: IpcId, ctx: &Context) -> Result<()> {
        self.message_ids.remove(id, |queue| {
            queue.remove(ctx)?;
            Ok(IpcRemoval::Destroy)
        })
    }

    /// Calls `op` with the semaphore set identified by `semid`.
    pub(crate) fn with_sem_set<T, F>(
        &self,
        semid: IpcId,
        required_perm: IpcAccess,
        ctx: &Context,
        op: F,
    ) -> Result<T>
    where
        F: FnOnce(&Arc<SemaphoreSet>) -> Result<T>,
    {
        self.sem_ids.with(semid, |sem_set| {
            sem_set
                .permission()
                .check_access(required_perm, ctx, &self.owner)?;
            op(sem_set)
        })?
    }

    pub(crate) fn remove_sem_set(&self, semid: IpcId, ctx: &Context) -> Result<()> {
        self.sem_ids.remove(semid, |sem_set| {
            sem_set.permission().check_owner(ctx, &self.owner)?;
            sem_set.mark_removed();
            Ok(IpcRemoval::Destroy)
        })
    }

    pub(crate) fn set_sem_permission(
        &self,
        semid: IpcId,
        input: &IpcPerm,
        ctx: &Context,
    ) -> Result<()> {
        self.sem_ids.update(semid, |sem_set| {
            sem_set.set_permission(input, ctx, &self.owner)
        })?
    }

    /// Returns the existing semaphore set or creates a new one.
    pub(crate) fn get_or_create_sem_set(
        &self,
        key: IpcKey,
        num_sems: usize,
        flags: IpcFlags,
        mode: u16,
        ctx: &Context,
    ) -> Result<IpcId> {
        let credentials = ctx.posix_thread.credentials();
        let validate_creation = if num_sems == 0 {
            Err(Error::with_message(
                Errno::EINVAL,
                "the number of semaphores is zero",
            ))
        } else {
            Ok(())
        };
        self.sem_ids.get_or_create(
            key,
            flags,
            validate_creation,
            |_| {
                Ok(Arc::new(SemaphoreSet::new(
                    key,
                    num_sems,
                    mode,
                    &credentials,
                )?))
            },
            |sem_set| {
                if sem_set.num_sems() < num_sems {
                    return_errno_with_message!(Errno::EINVAL, "the semaphore set is too small");
                }
                sem_set
                    .permission()
                    .check_access(IpcAccess::from_mode(mode), ctx, &self.owner)
            },
        )
    }
}

impl NsCommonOps for IpcNamespace {
    const TYPE: NsType = NsType::Ipc;

    fn owner_user_ns(&self) -> Option<&Arc<UserNamespace>> {
        Some(&self.owner)
    }

    fn parent(&self) -> Result<&Arc<Self>> {
        return_errno_with_message!(
            Errno::EINVAL,
            "an IPC namespace does not have a parent namespace"
        );
    }

    fn stashed_dentry(&self) -> &StashedDentry {
        &self.stashed_dentry
    }
}
