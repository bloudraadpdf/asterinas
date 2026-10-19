// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicU64, Ordering};

use aster_rights::ReadOp;

use super::{
    sem::{PendingBlocker, PendingOp, Semaphore, Status, update_pending_alter, wake_const_ops},
    undo::SemUndo,
};
use crate::{
    ipc::{IpcKey, IpcPerm, IpcPermission},
    prelude::*,
    process::{Credentials, PidIdentity, UserNamespace},
    time::clocks::RealTimeCoarseClock,
};

// The following constant values are derived from the default values in Linux.

/// Maximum number of semaphore sets.
pub(crate) const SEMMNI: usize = 32000;
/// Maximum number of semaphores per semaphore ID.
pub(crate) const SEMMSL: usize = 32000;
/// Maximum number of semaphores in all semaphore sets.
#[expect(dead_code)]
pub(crate) const SEMMNS: usize = SEMMNI * SEMMSL;
/// Maximum number of operations for semop.
pub(crate) const SEMOPM: usize = 500;
/// Maximum semaphore value.
pub(crate) const SEMVMX: i32 = 32767;
/// Maximum value that can be recorded for semaphore adjustment (SEM_UNDO).
pub(crate) const SEMAEM: i32 = SEMVMX;

#[derive(Debug)]
pub(crate) struct SemaphoreSet {
    /// Number of semaphores in the set
    num_sems: usize,
    /// Inner
    inner: Mutex<SemSetInner>,
    /// Creation time or last modification via `semctl`
    sem_ctime: AtomicU64,
    /// Last `semop` time
    sem_otime: AtomicU64,
}

// In Linux, most popular 64-bit architectures except x86_64 adopt the same
// layout of `semid_ds`.
// Reference: <https://elixir.bootlin.com/linux/v6.16.9/A/ident/semid64_ds>.
#[cfg(target_arch = "x86_64")]
#[padding_struct]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct SemidDs {
    pub(crate) sem_perm: IpcPerm,
    sem_otime: u64,
    _unused1: u64,
    sem_ctime: u64,
    _unused2: u64,
    sem_nsems: u64,
    _unused3: u64,
    _unused4: u64,
}

#[cfg(not(target_arch = "x86_64"))]
#[padding_struct]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct SemidDs {
    pub(crate) sem_perm: IpcPerm,
    sem_otime: u64,
    sem_ctime: u64,
    sem_nsems: u64,
    _unused3: u64,
    _unused4: u64,
}

#[derive(Debug)]
pub(super) struct SemSetInner {
    removed: bool,
    pub(super) undo: Vec<Weak<SemUndo>>,
    permission: IpcPermission,
    /// Semaphores
    pub(super) sems: Box<[Semaphore]>,
    /// Pending alter operations.
    pub(super) pending_alter: LinkedList<PendingOp>,
    /// Pending zeros operations.
    pub(super) pending_const: LinkedList<PendingOp>,
}

impl SemSetInner {
    pub(super) fn field_mut(
        &mut self,
    ) -> (
        &mut Box<[Semaphore]>,
        &mut LinkedList<PendingOp>,
        &mut LinkedList<PendingOp>,
    ) {
        (
            &mut self.sems,
            &mut self.pending_alter,
            &mut self.pending_const,
        )
    }
}

impl SemaphoreSet {
    /// Counts the number of pending operations waiting for the semaphore at `sem_num` to become
    /// zero.
    pub(crate) fn pending_zero_count(&self, sem_num: usize) -> Result<usize> {
        if sem_num >= self.num_sems {
            return_errno_with_message!(Errno::EINVAL, "the semaphore number is out of bounds");
        }

        let inner = self.inner.lock();
        let count_const = inner
            .pending_const
            .iter()
            .filter(|op| op.blocker(&inner.sems) == Ok(PendingBlocker::Zero(sem_num)))
            .count();
        let count_alter = inner
            .pending_alter
            .iter()
            .filter(|op| op.blocker(&inner.sems) == Ok(PendingBlocker::Zero(sem_num)))
            .count();
        Ok(count_const + count_alter)
    }

    /// Counts the number of pending operations waiting for the semaphore at `sem_num` to be able to
    /// decrease by a certain amount.
    pub(crate) fn pending_decrease_count(&self, sem_num: usize) -> Result<usize> {
        if sem_num >= self.num_sems {
            return_errno_with_message!(Errno::EINVAL, "the semaphore number is out of bounds");
        }

        let inner = self.inner.lock();
        let count = inner
            .pending_alter
            .iter()
            .filter(|op| op.blocker(&inner.sems) == Ok(PendingBlocker::Decrease(sem_num)))
            .count();
        Ok(count)
    }

    pub(crate) fn num_sems(&self) -> usize {
        self.num_sems
    }

    pub(crate) fn setval(&self, sem_num: usize, val: i32, pid: Arc<PidIdentity>) -> Result<()> {
        if sem_num >= self.num_sems {
            return_errno_with_message!(Errno::EINVAL, "the semaphore number is out of bounds");
        }
        if !(0..=SEMVMX).contains(&val) {
            return_errno_with_message!(Errno::ERANGE, "the semaphore value exceeds SEMVMX");
        }
        let mut inner = self.inner();
        let sem = &mut inner.sems[sem_num];
        sem.set_val(val);
        sem.set_latest_modified_pid(pid);
        self.clear_undo(&inner, Some(sem_num));
        self.wake_pending(&mut inner);
        self.update_ctime();
        Ok(())
    }

    pub(crate) fn values(&self) -> Result<Vec<u16>> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(self.num_sems)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        let inner = self.inner();
        values.extend(inner.sems.iter().map(|sem| sem.val() as u16));
        Ok(values)
    }

    pub(crate) fn set_values(&self, values: &[u16], pid: Arc<PidIdentity>) -> Result<()> {
        if values.len() != self.num_sems {
            return_errno_with_message!(Errno::EINVAL, "the semaphore array length is invalid");
        }
        if values.iter().any(|value| i32::from(*value) > SEMVMX) {
            return_errno_with_message!(Errno::ERANGE, "a semaphore value exceeds SEMVMX");
        }
        let mut inner = self.inner();
        for (sem, value) in inner.sems.iter_mut().zip(values) {
            sem.set_val(i32::from(*value));
            sem.set_latest_modified_pid(pid.clone());
        }
        self.clear_undo(&inner, None);
        self.wake_pending(&mut inner);
        self.update_ctime();
        Ok(())
    }

    fn clear_undo(&self, inner: &SemSetInner, index: Option<usize>) {
        for list in inner.undo.iter().filter_map(Weak::upgrade) {
            list.clear(self as *const _, index);
        }
    }

    pub(super) fn apply_undo(&self, list: &SemUndo, pid: Arc<PidIdentity>) {
        let mut inner = self.inner();
        let Some(adjustments) = list.take(self as *const _) else {
            return;
        };
        if inner.removed {
            return;
        }
        let mut changed = false;
        for (sem, adjustment) in inner.sems.iter_mut().zip(adjustments) {
            if adjustment != 0 {
                sem.set_val((sem.val() + adjustment).clamp(0, SEMVMX));
                sem.set_latest_modified_pid(pid.clone());
                changed = true;
            }
        }
        if changed {
            self.wake_pending(&mut inner);
        }
    }

    pub(in crate::ipc) fn mark_removed(&self) {
        self.inner().mark_removed(self as *const _);
    }

    fn wake_pending(&self, inner: &mut SemSetInner) {
        let (sems, pending_alter, pending_const) = inner.field_mut();
        let mut wake_queue = LinkedList::new();
        let has_completed = wake_const_ops(sems, pending_const, &mut wake_queue)
            | update_pending_alter(sems, pending_alter, pending_const, &mut wake_queue);
        for op in wake_queue {
            if let Some(waker) = op.waker() {
                waker.wake_up();
            }
        }
        if has_completed {
            self.update_otime();
        }
    }

    pub(crate) fn get<T>(&self, sem_num: usize, func: fn(&Semaphore) -> T) -> Result<T> {
        let inner = self.inner();
        let Some(sem) = inner.sems.get(sem_num) else {
            return_errno_with_message!(Errno::EINVAL, "the semaphore number is out of bounds");
        };

        let result = func(sem);
        Ok(result)
    }

    pub(crate) fn permission(&self) -> IpcPermission {
        self.inner.lock().permission
    }

    pub(in crate::ipc) fn set_permission(
        &self,
        input: &IpcPerm,
        ctx: &Context,
        owner: &UserNamespace,
    ) -> Result<()> {
        let mut inner = self.inner.lock();
        inner.permission.check_owner(ctx, owner)?;
        inner.permission.update(input)?;
        self.update_ctime();
        Ok(())
    }

    fn update_ctime(&self) {
        self.sem_ctime.store(
            RealTimeCoarseClock::get().read_time().as_secs(),
            Ordering::Relaxed,
        );
    }

    pub(super) fn update_otime(&self) {
        self.sem_otime.store(
            RealTimeCoarseClock::get().read_time().as_secs(),
            Ordering::Relaxed,
        );
    }

    pub(super) fn inner(&self) -> MutexGuard<'_, SemSetInner> {
        self.inner.lock()
    }

    pub(in crate::ipc) fn new(
        key: IpcKey,
        num_sems: usize,
        mode: u16,
        credentials: &Credentials<ReadOp>,
    ) -> Result<Self> {
        if num_sems == 0 {
            return_errno_with_message!(Errno::EINVAL, "the number of semaphores is zero")
        }
        if num_sems > SEMMSL {
            return_errno_with_message!(Errno::EINVAL, "the number of semaphores exceeds SEMMSL");
        }

        let mut sems = Vec::new();
        sems.try_reserve_exact(num_sems)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for _ in 0..num_sems {
            sems.push(Semaphore::new(0));
        }

        let permission = IpcPermission::new(key, credentials.euid(), credentials.egid(), mode);

        Ok(Self {
            num_sems,
            inner: Mutex::new(SemSetInner {
                removed: false,
                undo: Vec::new(),
                permission,
                sems: sems.into_boxed_slice(),
                pending_alter: LinkedList::new(),
                pending_const: LinkedList::new(),
            }),
            sem_ctime: AtomicU64::new(RealTimeCoarseClock::get().read_time().as_secs()),
            sem_otime: AtomicU64::new(0),
        })
    }

    pub(crate) fn semid_ds(&self) -> SemidDs {
        let inner = self.inner.lock();
        let ipc_perm = inner.permission.abi();

        SemidDs {
            sem_perm: ipc_perm,
            sem_otime: self.sem_otime.load(Ordering::Relaxed),
            sem_ctime: self.sem_ctime.load(Ordering::Relaxed),
            sem_nsems: self.num_sems as u64,
            ..SemidDs::default()
        }
    }
}

impl SemSetInner {
    fn mark_removed(&mut self, set: *const SemaphoreSet) {
        if self.removed {
            return;
        }
        self.removed = true;
        for queue in [&mut self.pending_alter, &mut self.pending_const] {
            for op in core::mem::take(queue) {
                op.set_status(Status::Removed);
                if let Some(waker) = op.waker() {
                    waker.wake_up();
                }
            }
        }
        for list in self.undo.drain(..).filter_map(|list| list.upgrade()) {
            list.forget(set);
        }
    }
}

impl Drop for SemaphoreSet {
    fn drop(&mut self) {
        let key = self as *const _;
        self.inner.get_mut().mark_removed(key);
    }
}
