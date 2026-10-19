// SPDX-License-Identifier: MPL-2.0

use super::{
    sem::{SemBuf, Status},
    sem_set::{SEMAEM, SemSetInner, SemaphoreSet},
};
use crate::{ipc::IpcFlags, prelude::*, process::PidIdentity};

pub(crate) struct SemUndoOwner {
    list: Arc<SemUndo>,
    pid: Arc<PidIdentity>,
}

#[derive(Debug)]
pub(crate) struct SemUndo {
    inner: Mutex<UndoState>,
}

#[derive(Debug)]
struct UndoState {
    // Temporary operation references do not own exit adjustments.
    owners: usize,
    entries: Vec<UndoEntry>,
}

#[derive(Debug)]
struct UndoEntry {
    set: Weak<SemaphoreSet>,
    adjustments: Vec<i32>,
}

pub(super) struct UndoRef {
    list: Weak<SemUndo>,
    set: Weak<SemaphoreSet>,
}

impl SemUndoOwner {
    pub(crate) fn new(pid: Arc<PidIdentity>) -> Self {
        Self {
            list: Arc::new(SemUndo {
                inner: Mutex::new(UndoState {
                    owners: 1,
                    entries: Vec::new(),
                }),
            }),
            pid,
        }
    }

    pub(crate) fn share_for(&self, pid: Arc<PidIdentity>) -> Self {
        let list = self.list.clone();
        list.inner.lock().owners += 1;
        Self { list, pid }
    }

    pub(crate) fn access(&self) -> Arc<SemUndo> {
        self.list.clone()
    }
}

impl Drop for SemUndoOwner {
    fn drop(&mut self) {
        {
            let mut state = self.list.inner.lock();
            state.owners -= 1;
            if state.owners != 0 {
                return;
            }
        }
        loop {
            // Set state precedes undo state; release this lock before applying.
            let next = self
                .list
                .inner
                .lock()
                .entries
                .first()
                .map(|entry| entry.set.clone());
            let Some(next) = next else { break };
            if let Some(set) = next.upgrade() {
                set.apply_undo(&self.list, self.pid.clone());
            } else {
                self.list.forget(next.as_ptr());
            }
        }
    }
}

impl SemUndo {
    pub(super) fn register(
        self: &Arc<Self>,
        set: &Arc<SemaphoreSet>,
        inner: &mut SemSetInner,
    ) -> Result<UndoRef> {
        let mut state = self.inner.lock();
        state.entries.retain(|entry| entry.set.strong_count() != 0);
        let key = Arc::as_ptr(set);
        if !state.entries.iter().any(|entry| entry.set.as_ptr() == key) {
            let mut adjustments = Vec::new();
            adjustments
                .try_reserve_exact(set.num_sems())
                .map_err(|_| Error::new(Errno::ENOMEM))?;
            adjustments.resize(set.num_sems(), 0);
            state
                .entries
                .try_reserve(1)
                .map_err(|_| Error::new(Errno::ENOMEM))?;
            inner.undo.retain(|list| list.strong_count() != 0);
            inner
                .undo
                .try_reserve(1)
                .map_err(|_| Error::new(Errno::ENOMEM))?;
            state.entries.push(UndoEntry {
                set: Arc::downgrade(set),
                adjustments,
            });
            inner.undo.push(Arc::downgrade(self));
        }
        Ok(UndoRef {
            list: Arc::downgrade(self),
            set: Arc::downgrade(set),
        })
    }

    pub(super) fn clear(&self, set: *const SemaphoreSet, index: Option<usize>) {
        let mut state = self.inner.lock();
        if let Some(entry) = state
            .entries
            .iter_mut()
            .find(|entry| entry.set.as_ptr() == set)
        {
            if let Some(index) = index {
                entry.adjustments[index] = 0;
            } else {
                entry.adjustments.fill(0);
            }
        }
    }

    pub(super) fn take(&self, set: *const SemaphoreSet) -> Option<Vec<i32>> {
        let mut state = self.inner.lock();
        let index = state
            .entries
            .iter()
            .position(|entry| entry.set.as_ptr() == set)?;
        Some(state.entries.swap_remove(index).adjustments)
    }

    pub(super) fn forget(&self, set: *const SemaphoreSet) {
        let _ = self.take(set);
    }
}

impl UndoRef {
    pub(super) fn apply(&self, operations: &[SemBuf]) -> core::result::Result<(), Status> {
        let list = self.list.upgrade().ok_or(Status::Removed)?;
        let mut state = list.inner.lock();
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| Weak::ptr_eq(&entry.set, &self.set))
            .ok_or(Status::Removed)?;
        let uses_undo = |op: &SemBuf| {
            IpcFlags::from_bits_truncate(op.sem_flags as u32).contains(IpcFlags::SEM_UNDO)
        };
        for (index, op) in operations
            .iter()
            .enumerate()
            .filter(|(_, op)| uses_undo(op))
        {
            let change: i32 = operations[..=index]
                .iter()
                .filter(|prior| prior.sem_num == op.sem_num && uses_undo(prior))
                .map(|prior| i32::from(prior.sem_op))
                .sum();
            let adjusted = entry.adjustments[op.sem_num as usize] - change;
            if !(-SEMAEM..=SEMAEM).contains(&adjusted) {
                return Err(Status::ErrValRange);
            }
        }
        for op in operations.iter().filter(|op| uses_undo(op)) {
            entry.adjustments[op.sem_num as usize] -= i32::from(op.sem_op);
        }
        Ok(())
    }
}
