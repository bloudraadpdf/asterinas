// SPDX-License-Identifier: MPL-2.0

use super::{IpcAccess, IpcId, IpcKey, IpcNamespace, IpcPerm, IpcPermission};
use crate::{
    prelude::*,
    process::{PidIdentity, Process, UserNamespace},
    time::clocks::RealTimeCoarseClock,
    util::quota::{Quota, QuotaCharge},
    vm::{
        page_cache::{Vmo, VmoOptions},
        perms::VmPerms,
    },
};

pub(super) const SHMMNI: usize = 4096;
pub(super) const SHMMAX: usize = usize::MAX - (1 << 24);
pub(super) const SHMALL: usize = SHMMAX;
const SHM_DEST: u16 = 0o1000;
pub(crate) const SHM_RDONLY: u32 = 0o10000;
pub(crate) const SHM_RND: u32 = 0o20000;
pub(crate) const SHM_REMAP: u32 = 0o40000;
const SHM_EXEC: u32 = 0o100000;

pub(crate) struct SharedSegment {
    id: IpcId,
    size: usize,
    vmo: Arc<Vmo>,
    creator: Arc<PidIdentity>,
    inner: Mutex<SegmentState>,
    _pages: QuotaCharge,
}

struct SegmentState {
    permission: IpcPermission,
    attached: usize,
    reserved: usize,
    last_pid: Option<Arc<PidIdentity>>,
    attached_at: u64,
    detached_at: u64,
    changed_at: u64,
}

#[padding_struct]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct ShmidDs {
    permission: IpcPerm,
    size: u64,
    attached_at: u64,
    detached_at: u64,
    changed_at: u64,
    creator: i32,
    last_pid: i32,
    attached: u64,
    _unused1: u64,
    _unused2: u64,
}

struct AttachmentGroup {
    segment: Arc<SharedSegment>,
    namespace: Arc<IpcNamespace>,
    creator: Arc<PidIdentity>,
}

pub(crate) struct PendingAttachment {
    group: Option<Arc<AttachmentGroup>>,
    perms: VmPerms,
    may_perms: VmPerms,
}

pub(crate) struct ShmMapping {
    group: Arc<AttachmentGroup>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShmAttachmentId(usize);

fn now() -> u64 {
    RealTimeCoarseClock::get().read_time().as_secs()
}

impl SharedSegment {
    pub(super) fn new(
        id: IpcId,
        key: IpcKey,
        size: usize,
        mode: u16,
        quota: &Arc<Quota>,
        ctx: &Context,
    ) -> Result<Self> {
        let rounded = size
            .checked_add(PAGE_SIZE - 1)
            .ok_or_else(|| Error::new(Errno::EINVAL))?
            & !(PAGE_SIZE - 1);
        let pages = quota.charge(rounded / PAGE_SIZE)?;
        let vmo = VmoOptions::new(rounded).alloc()?;
        let credentials = ctx.posix_thread.credentials();
        Ok(Self {
            id,
            size,
            vmo,
            creator: ctx.process.pid_identity().clone(),
            inner: Mutex::new(SegmentState {
                permission: IpcPermission::new(key, credentials.euid(), credentials.egid(), mode),
                attached: 0,
                reserved: 0,
                last_pid: None,
                attached_at: 0,
                detached_at: 0,
                changed_at: now(),
            }),
            _pages: pages,
        })
    }

    pub(super) fn check_lookup(
        &self,
        size: usize,
        mode: u16,
        ctx: &Context,
        owner: &UserNamespace,
    ) -> Result<()> {
        if size > self.size {
            return_errno_with_message!(Errno::EINVAL, "the shared segment is too small");
        }
        self.inner
            .lock()
            .permission
            .check_access(IpcAccess::from_mode(mode), ctx, owner)
    }

    pub(crate) fn status(&self, ctx: &Context, owner: &UserNamespace) -> Result<ShmidDs> {
        let state = self.inner.lock();
        state.permission.check_access(IpcAccess::READ, ctx, owner)?;
        let pid_ns = ctx.process.pid_namespace();
        Ok(ShmidDs {
            permission: state.permission.abi(),
            size: self.size as u64,
            attached_at: state.attached_at,
            detached_at: state.detached_at,
            changed_at: state.changed_at,
            creator: self.creator.in_namespace(pid_ns) as i32,
            last_pid: state
                .last_pid
                .as_ref()
                .map_or(0, |pid| pid.in_namespace(pid_ns) as i32),
            attached: state.attached as u64,
            ..ShmidDs::default()
        })
    }

    pub(super) fn set_status(
        &self,
        input: &ShmidDs,
        ctx: &Context,
        owner: &UserNamespace,
    ) -> Result<()> {
        let mut state = self.inner.lock();
        state.permission.check_owner(ctx, owner)?;
        state.permission.update(&input.permission)?;
        state.changed_at = now();
        Ok(())
    }

    pub(super) fn mark_removed(&self, ctx: &Context, owner: &UserNamespace) -> Result<bool> {
        let mut state = self.inner.lock();
        state.permission.check_owner(ctx, owner)?;
        state.permission.mode |= SHM_DEST;
        state.permission.key = 0;
        Ok(state.attached == 0 && state.reserved == 0)
    }

    pub(super) fn ready_to_destroy(&self) -> bool {
        let state = self.inner.lock();
        state.permission.mode & SHM_DEST != 0 && state.attached == 0 && state.reserved == 0
    }

    pub(super) fn reserve(
        self: &Arc<Self>,
        namespace: &Arc<IpcNamespace>,
        flags: u32,
        ctx: &Context,
        owner: &UserNamespace,
    ) -> Result<PendingAttachment> {
        let readonly = flags & SHM_RDONLY != 0;
        let mut access = IpcAccess::READ;
        let mut perms = VmPerms::READ;
        let mut may_perms = VmPerms::MAY_READ | VmPerms::MAY_EXEC;
        if !readonly {
            access |= IpcAccess::WRITE;
            perms |= VmPerms::WRITE;
            may_perms |= VmPerms::MAY_WRITE;
        }
        if flags & SHM_EXEC != 0 {
            access |= IpcAccess::EXECUTE;
            perms |= VmPerms::EXEC;
        }
        let mut state = self.inner.lock();
        if state.permission.mode & SHM_DEST != 0 && state.attached == 0 && state.reserved == 0 {
            return_errno_with_message!(Errno::EIDRM, "the shared segment was removed");
        }
        state.permission.check_access(access, ctx, owner)?;
        state.reserved += 1;
        Ok(PendingAttachment {
            group: Some(Arc::new(AttachmentGroup {
                segment: self.clone(),
                namespace: namespace.clone(),
                creator: ctx.process.pid_identity().clone(),
            })),
            perms,
            may_perms,
        })
    }
}

impl AttachmentGroup {
    fn actor(&self) -> Arc<PidIdentity> {
        Process::current().map_or_else(
            || self.creator.clone(),
            |process| process.pid_identity().clone(),
        )
    }

    fn release(&self, pending: bool) {
        {
            let mut state = self.segment.inner.lock();
            if pending {
                state.reserved -= 1;
            } else {
                state.attached -= 1;
                state.last_pid = Some(self.actor());
                state.detached_at = now();
            }
        }
        // Namespace removal takes the ID lock before the segment lock.
        self.namespace.finalize_shm(self.segment.id, &self.segment);
    }

    fn open(&self) {
        let mut state = self.segment.inner.lock();
        state.attached += 1;
        state.last_pid = Some(self.actor());
        state.attached_at = now();
    }
}

impl PendingAttachment {
    pub(crate) fn vmo(&self) -> &Arc<Vmo> {
        &self.group.as_ref().unwrap().segment.vmo
    }

    pub(crate) fn perms(&self) -> VmPerms {
        self.perms
    }
    pub(crate) fn may_perms(&self) -> VmPerms {
        self.may_perms
    }

    pub(crate) fn publish(mut self) -> ShmMapping {
        let group = self.group.take().unwrap();
        {
            let mut state = group.segment.inner.lock();
            state.reserved -= 1;
            state.attached += 1;
            state.last_pid = Some(group.actor());
            state.attached_at = now();
        }
        ShmMapping { group }
    }
}

impl Drop for PendingAttachment {
    fn drop(&mut self) {
        if let Some(group) = &self.group {
            group.release(true);
        }
    }
}

impl ShmMapping {
    pub(crate) fn id(&self) -> ShmAttachmentId {
        ShmAttachmentId(Arc::as_ptr(&self.group) as usize)
    }
}

impl Clone for ShmMapping {
    fn clone(&self) -> Self {
        self.group.open();
        Self {
            group: self.group.clone(),
        }
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        self.group.release(false);
    }
}
