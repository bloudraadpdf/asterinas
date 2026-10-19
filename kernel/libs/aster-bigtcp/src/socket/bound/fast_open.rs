// SPDX-License-Identifier: MPL-2.0

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use aes::{
    Aes128,
    cipher::{BlockEncrypt, NewBlockCipher},
};
use smoltcp::{
    socket::tcp::FastOpenAdmission,
    time::Instant,
    wire::{IpAddress, TcpFastOpenCookie},
};
use subtle::ConstantTimeEq;

use crate::{ext::Ext, socket::unbound::RawTcpSocket};

pub(super) struct FastOpenQueue {
    limit: usize,
    pending: Arc<AtomicUsize>,
    key: Option<CookieKey>,
}

struct CookieKey {
    cipher: Aes128,
    created: Instant,
}

pub(super) struct PendingFastOpen {
    count: Arc<AtomicUsize>,
}

impl FastOpenQueue {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            pending: Arc::new(AtomicUsize::new(0)),
            key: None,
        }
    }

    pub(super) fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    pub(super) fn prepare<E: Ext>(
        &mut self,
        socket: &mut RawTcpSocket,
        now: Instant,
        source: IpAddress,
        incoming: Option<TcpFastOpenCookie>,
        payload_len: usize,
    ) -> Option<PendingFastOpen> {
        socket.disable_fast_open().unwrap();
        let incoming = incoming?;
        if self.limit == 0 || self.pending.load(Ordering::Acquire) >= self.limit {
            return None;
        }
        let cookie = self.cookie::<E>(now, source);
        let valid = bool::from(incoming.as_bytes().ct_eq(cookie.as_bytes()));
        let reservation = if valid && payload_len > 0 {
            self.pending
                .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                    (pending < self.limit).then(|| pending + 1)
                })
                .ok()?;
            Some(PendingFastOpen {
                count: self.pending.clone(),
            })
        } else {
            None
        };
        let admission = if reservation.is_some() {
            FastOpenAdmission::Accept
        } else {
            FastOpenAdmission::Deny
        };
        socket.prepare_fast_open(cookie, admission).unwrap();
        reservation
    }

    // RFC 7413 section 4.1.2: AES over the client address, with expiring keys.
    fn cookie<E: Ext>(&mut self, now: Instant, source: IpAddress) -> TcpFastOpenCookie {
        const KEY_LIFETIME_MILLIS: i64 = 60 * 60 * 1000;
        if self.key.as_ref().is_none_or(|key| {
            now < key.created
                || now
                    .total_millis()
                    .saturating_sub(key.created.total_millis())
                    >= KEY_LIFETIME_MILLIS
        }) {
            let mut bytes = [0; 16];
            E::fill_random(&mut bytes);
            self.key = Some(CookieKey {
                cipher: Aes128::new((&bytes).into()),
                created: now,
            });
        }
        let address = match source {
            IpAddress::Ipv4(address) => address.to_ipv6_mapped().octets(),
            IpAddress::Ipv6(address) => address.octets(),
        };
        let mut block = address.into();
        self.key.as_ref().unwrap().cipher.encrypt_block(&mut block);
        TcpFastOpenCookie::new(&block[..8]).unwrap()
    }
}

impl Drop for PendingFastOpen {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::*;
    use crate::{iface::tests::TestExt, socket::unbound::new_tcp_socket};

    #[ktest]
    fn cookies_authenticate_addresses_and_expire_on_packet_arrival() {
        let mut queue = FastOpenQueue::new(1);
        let first: IpAddress = core::net::Ipv4Addr::new(192, 0, 2, 1).into();
        let second: IpAddress = core::net::Ipv4Addr::new(192, 0, 2, 2).into();
        let now = Instant::from_secs(0);
        let cookie = queue.cookie::<TestExt>(now, first);
        assert_eq!(
            cookie,
            queue.cookie::<TestExt>(Instant::from_secs(3599), first)
        );
        assert_ne!(cookie, queue.cookie::<TestExt>(now, second));
        assert_ne!(
            cookie,
            queue.cookie::<TestExt>(Instant::from_secs(3600), first)
        );
    }

    #[ktest]
    fn pending_opens_release_capacity_through_owned_reservations() {
        let mut queue = FastOpenQueue::new(1);
        let source: IpAddress = core::net::Ipv4Addr::LOCALHOST.into();
        let now = Instant::from_secs(0);
        let cookie = queue.cookie::<TestExt>(now, source);
        let mut socket = new_tcp_socket();
        socket.listen(80).unwrap();
        let pending = queue
            .prepare::<TestExt>(&mut socket, now, source, Some(cookie), 1)
            .unwrap();
        assert!(
            queue
                .prepare::<TestExt>(&mut socket, now, source, Some(cookie), 1)
                .is_none()
        );
        drop(pending);
        assert!(
            queue
                .prepare::<TestExt>(&mut socket, now, source, Some(cookie), 1)
                .is_some()
        );
        assert_eq!(queue.pending.load(Ordering::Acquire), 0);
        queue.set_limit(0);
        assert!(
            queue
                .prepare::<TestExt>(&mut socket, now, source, Some(cookie), 1)
                .is_none()
        );
    }
}
