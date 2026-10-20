// SPDX-License-Identifier: MPL-2.0

//! Rename validation for scoped path walks, including nested overlay renames.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::prelude::*;

static RENAMES: RenameState = RenameState::new();

struct RenameState {
    generation: AtomicU64,
    active_writers: AtomicUsize,
}

impl RenameState {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active_writers: AtomicUsize::new(0),
        }
    }

    fn mutation(&'static self) -> RenameMutation {
        self.active_writers.fetch_add(1, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        RenameMutation(self)
    }

    fn epoch(&'static self) -> Result<RenameEpoch> {
        let epoch = RenameEpoch {
            state: self,
            generation: self.generation.load(Ordering::SeqCst),
        };
        epoch.validate()?;
        Ok(epoch)
    }
}

pub(super) struct RenameMutation(&'static RenameState);

impl RenameMutation {
    pub(super) fn new() -> Self {
        RENAMES.mutation()
    }
}

impl Drop for RenameMutation {
    fn drop(&mut self) {
        self.0.active_writers.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) struct RenameEpoch {
    state: &'static RenameState,
    generation: u64,
}

impl RenameEpoch {
    pub(super) fn begin() -> Result<Self> {
        RENAMES.epoch()
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.state.active_writers.load(Ordering::SeqCst) != 0
            || self.state.generation.load(Ordering::SeqCst) != self.generation
        {
            return_errno_with_message!(Errno::EAGAIN, "a rename overlaps the scoped path lookup");
        }
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn nested_writers_and_finished_mutations_invalidate_a_reader() {
        static STATE: RenameState = RenameState::new();
        let before = STATE.epoch().unwrap();
        let first = STATE.mutation();
        let second = STATE.mutation();
        assert_eq!(STATE.epoch().err().unwrap().error(), Errno::EAGAIN);
        drop(first);
        assert_eq!(STATE.epoch().err().unwrap().error(), Errno::EAGAIN);
        drop(second);
        assert_eq!(before.validate().err().unwrap().error(), Errno::EAGAIN);
        STATE.epoch().unwrap().validate().unwrap();
    }
}
