// SPDX-License-Identifier: MPL-2.0

use super::sched::PollScheduler;
use crate::net::socket::{
    ip::{DatagramObserver, StreamObserver},
    util::SocketTimeouts,
};

pub(in crate::net) struct BigtcpExt;

impl aster_bigtcp::ext::Ext for BigtcpExt {
    type ScheduleNextPoll = PollScheduler;

    type TcpEventObserver = StreamObserver;
    type TcpAcceptContext = SocketTimeouts;

    fn tcp_accept_context(observer: &StreamObserver) -> SocketTimeouts {
        observer.accept_timeouts()
    }
    type UdpEventObserver = DatagramObserver;

    fn receive_timestamp() -> core::time::Duration {
        crate::time::SystemTime::realtime_at(aster_time::read_monotonic_time())
    }

    fn fill_random(bytes: &mut [u8]) {
        crate::util::random::getrandom(bytes);
    }
}
