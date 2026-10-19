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

    pub(crate) fn charge(self: &Arc<Self>, units: usize) -> Result<QuotaCharge> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(units).filter(|next| *next <= self.limit)
            })
            .map_err(|_| Error::with_message(Errno::ENOSPC, "the resource quota is exhausted"))?;
        Ok(QuotaCharge {
            quota: self.clone(),
            units,
        })
    }

    pub(crate) fn remaining(&self) -> usize {
        self.limit - self.used.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct QuotaCharge {
    quota: Arc<Quota>,
    units: usize,
}

impl Drop for QuotaCharge {
    fn drop(&mut self) {
        self.quota.used.fetch_sub(self.units, Ordering::Relaxed);
    }
}
