// SPDX-License-Identifier: MPL-2.0

use ostd::sync::{WaitQueue, Waker};

use crate::{
    events::IoEvents,
    prelude::*,
    process::signal::{PollHandle, Pollee},
};

pub(super) struct SendBuffer {
    usage: SpinLock<Usage>,
    pollee: Pollee,
    waiters: WaitQueue,
}

struct Usage {
    limit: usize,
    used: usize,
}

pub(super) struct SendCharge {
    buffer: Arc<SendBuffer>,
    bytes: usize,
}

impl SendBuffer {
    pub(super) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            usage: SpinLock::new(Usage { limit, used: 0 }),
            pollee: Pollee::new(),
            waiters: WaitQueue::new(),
        })
    }

    pub(super) fn limit(&self) -> usize {
        self.usage.lock().limit
    }

    pub(super) fn set_limit(&self, limit: usize) {
        self.usage.lock().limit = limit;
        self.pollee.invalidate();
        self.waiters.wake_all();
    }

    pub(super) fn charge(
        self: &Arc<Self>,
        payload: usize,
        auxiliary_bytes: usize,
    ) -> Result<SendCharge> {
        let bytes = {
            let mut usage = self.usage.lock();
            if payload > usage.limit.saturating_sub(32) {
                return_errno_with_message!(Errno::EMSGSIZE, "the datagram exceeds the send buffer");
            }
            if usage.used >= usage.limit {
                return_errno_with_message!(Errno::EAGAIN, "the send buffer is full");
            }
            let allocation = packet_charge(payload, auxiliary_bytes)
                .ok_or_else(|| Error::new(Errno::ENOBUFS))?;
            usage.used = usage
                .used
                .checked_add(allocation)
                .ok_or_else(|| Error::new(Errno::ENOBUFS))?;
            allocation
        };
        self.pollee.invalidate();
        Ok(SendCharge {
            buffer: self.clone(),
            bytes,
        })
    }

    pub(super) fn register(&self, waker: &Arc<Waker>) {
        self.waiters.enqueue(waker.clone());
    }

    pub(super) fn is_writable(&self) -> bool {
        let usage = self.usage.lock();
        usage.used < usage.limit / 4
    }

    pub(super) fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        self.pollee.poll_with(mask & IoEvents::OUT, poller, || {
            if self.is_writable() {
                IoEvents::OUT
            } else {
                IoEvents::empty()
            }
        })
    }
}

// Linux charges aligned, size-class-rounded packet storage rather than payload bytes.
// Reference: https://codebrowser.dev/linux/linux/net/core/skbuff.c.html#kmalloc_reserve
fn packet_charge(payload: usize, auxiliary_bytes: usize) -> Option<usize> {
    const DATA_ALIGNMENT: usize = 64;
    const SHARED_INFO_BYTES: usize = 320;
    const PACKET_METADATA_BYTES: usize = 256;
    const MAX_LINEAR_BYTES: usize = (PAGE_SIZE << 2) - SHARED_INFO_BYTES;
    const MAX_FRAGMENT_BYTES: usize = 17 * PAGE_SIZE;
    let fragments = payload
        .saturating_sub(MAX_LINEAR_BYTES)
        .min(MAX_FRAGMENT_BYTES);
    let fragments = fragments.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    let linear = payload.checked_sub(fragments)?;
    let aligned = linear.checked_add(DATA_ALIGNMENT - 1)? & !(DATA_ALIGNMENT - 1);
    aligned
        .checked_add(SHARED_INFO_BYTES)?
        .checked_next_power_of_two()?
        .checked_add(PACKET_METADATA_BYTES)?
        .checked_add(fragments)?
        .checked_add(auxiliary_bytes)
}

impl Drop for SendCharge {
    fn drop(&mut self) {
        let writable = {
            let mut usage = self.buffer.usage.lock();
            usage.used -= self.bytes;
            usage.used < usage.limit / 4
        };
        if writable {
            self.buffer.pollee.notify(IoEvents::OUT);
            self.buffer.waiters.wake_all();
        }
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn empty_datagrams_have_finite_memory_charges() {
        let buffer = SendBuffer::new(4608);
        let charges: Vec<_> = (0..6).map(|_| buffer.charge(0, 0).unwrap()).collect();
        assert!(
            buffer
                .charge(0, 0)
                .is_err_and(|error| error.error() == Errno::EAGAIN)
        );
        drop(charges);
        assert!(buffer.is_writable());
        assert_eq!(buffer.usage.lock().used, 0);
    }

    #[ktest]
    fn one_datagram_can_cross_the_memory_watermark() {
        let buffer = SendBuffer::new(8192);
        let packet = buffer.charge(8000, 0).unwrap();
        assert!(!buffer.is_writable());
        assert!(
            buffer
                .charge(1, 0)
                .is_err_and(|error| error.error() == Errno::EAGAIN)
        );
        drop(packet);
        assert!(buffer.is_writable());
        assert!(
            buffer
                .charge(8161, 0)
                .is_err_and(|error| error.error() == Errno::EMSGSIZE)
        );
    }

    #[ktest]
    fn packet_charges_match_linux_allocation_boundaries() {
        for (payload, expected) in [
            (0, 768),
            (128, 768),
            (256, 1280),
            (768, 2304),
            (1500, 2304),
            (2048, 4352),
            (3500, 4352),
            (4000, 8448),
            (4576, 8448),
            (8192, 16640),
            (16064, 16640),
            (16065, 20736),
            (20000, 20736),
            (32000, 33024),
            (64000, 65792),
            (65504, 69888),
            (96000, 102656),
            (131072, 135424),
            (200000, 200960),
            (400000, 594176),
        ] {
            assert_eq!(packet_charge(payload, 0), Some(expected));
        }
        assert_eq!(packet_charge(4000, 64), Some(8512));
        assert_eq!(packet_charge(usize::MAX, 0), None);
        let buffer = SendBuffer::new(4608);
        let charge = buffer.charge(4000, 0).unwrap();
        assert!(
            buffer
                .charge(4000, 0)
                .is_err_and(|error| error.error() == Errno::EAGAIN)
        );
        drop(charge);
        assert!(buffer.is_writable());
    }
}
