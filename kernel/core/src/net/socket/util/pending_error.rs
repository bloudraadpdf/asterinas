// SPDX-License-Identifier: MPL-2.0

use crate::{
    events::IoEvents,
    prelude::*,
    process::signal::{PollHandle, Pollee},
};

pub(crate) struct PendingError {
    error: SpinLock<Option<Error>>,
    pollee: Pollee,
}

impl PendingError {
    pub(crate) fn new() -> Self {
        Self {
            error: SpinLock::new(None),
            pollee: Pollee::new(),
        }
    }

    pub(crate) fn set(&self, error: Error) {
        let mut pending = self.error.lock();
        *pending = Some(error);
        self.pollee.notify(IoEvents::ERR);
    }

    pub(crate) fn take(&self) -> Option<Error> {
        let mut pending = self.error.lock();
        let error = pending.take();
        self.pollee.invalidate();
        error
    }

    pub(crate) fn check(&self) -> Result<()> {
        match self.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn poll(&self, poller: Option<&mut PollHandle>) -> IoEvents {
        self.pollee.poll_with(IoEvents::ERR, poller, || {
            if self.error.lock().is_some() {
                IoEvents::ERR
            } else {
                IoEvents::empty()
            }
        })
    }

    pub(crate) fn poll_with(
        &self,
        mask: IoEvents,
        mut poller: Option<&mut PollHandle>,
        pollee: &Pollee,
        check: impl FnOnce() -> IoEvents,
    ) -> IoEvents {
        self.poll(poller.as_deref_mut()) | pollee.poll_with(mask, poller, check)
    }
}
