// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::prelude::*;

/// A shared limit whose reservations remain owned until their resource is released.
#[derive(Debug)]
pub(crate) struct Quota {
    limit: usize,
    used: AtomicUsize,
}

impl Quota {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    pub(crate) fn charge(self: &Arc<Self>) -> Result<QuotaCharge> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(1).filter(|next| *next <= self.limit)
            })
            .map_err(|_| Error::with_message(Errno::ENOSPC, "the resource quota is exhausted"))?;
        Ok(QuotaCharge(self.clone()))
    }

    pub(crate) fn remaining(&self) -> usize {
        self.limit - self.used.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct QuotaCharge(Arc<Quota>);

impl Drop for QuotaCharge {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::Relaxed);
    }
}
