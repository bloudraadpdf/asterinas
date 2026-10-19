// SPDX-License-Identifier: MPL-2.0

use alloc::{sync::Arc, vec::Vec};

use aster_softirq::BottomHalfDisabled;
use ostd::{prelude::ktest, sync::SpinLock};

use super::{
    BindPortConfig, BoundTcpPort, Iface, InterfaceFlags, InterfaceName, InterfaceType, IpIface,
    ScheduleNextPoll,
};
use crate::{
    device::{AnyNetworkDevice, Loopback, WithDevice},
    errors::{BindError, tcp::ListenError},
    ext::Ext,
    socket::{ConnectState, RawTcpOption, RawTcpSetOption, TcpConnection, TcpListener},
    wire::{IpEndpoint, Ipv4Address, Ipv4Cidr},
};

pub(crate) struct TestExt;
pub(crate) struct NoScheduler;
struct TestDevice(SpinLock<Loopback, BottomHalfDisabled>);

impl Ext for TestExt {
    type ScheduleNextPoll = NoScheduler;
    type TcpEventObserver = ();
    type TcpAcceptContext = ();

    fn tcp_accept_context(_observer: &Self::TcpEventObserver) {}
    type UdpEventObserver = ();

    fn receive_timestamp() -> core::time::Duration {
        static NEXT_STAMP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
        core::time::Duration::from_nanos(
            NEXT_STAMP.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
        )
    }

    fn fill_random(bytes: &mut [u8]) {
        static NEXT_KEY: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(1);
        bytes.fill(NEXT_KEY.fetch_add(1, core::sync::atomic::Ordering::Relaxed));
    }
}

impl ScheduleNextPoll for NoScheduler {
    fn schedule_next_poll(&self, _ms: Option<u64>) {}
}

impl WithDevice for TestDevice {
    type Device = Loopback;

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut dyn AnyNetworkDevice) -> R,
    {
        f(&mut *self.0.lock())
    }
}

fn ifaces() -> Vec<Arc<dyn Iface<TestExt>>> {
    [1, 2]
        .into_iter()
        .map(|last| {
            IpIface::new(
                TestDevice(SpinLock::new(Loopback::new())),
                Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, last), 24),
                None,
                InterfaceName::from_str_truncated("test"),
                NoScheduler,
                InterfaceType::LOOPBACK,
                InterfaceFlags::UP | InterfaceFlags::LOOPBACK,
            ) as Arc<dyn Iface<TestExt>>
        })
        .collect()
}

fn options() -> RawTcpOption {
    RawTcpOption {
        keep_alive: None,
        is_nagle_enabled: true,
        fast_open_backlog: 0,
        fast_open_retries: core::num::NonZeroU8::new(6).unwrap(),
    }
}

fn any(port: u16) -> IpEndpoint {
    IpEndpoint::new(Ipv4Address::UNSPECIFIED.into(), port)
}

#[ktest]
fn multi_iface_port_reservation_rolls_back_and_retries_ephemeral_ports() {
    let ifaces = ifaces();
    let blocked = ifaces[1]
        .bind_tcp(BindPortConfig::new(any(32768), false))
        .unwrap();
    assert!(matches!(
        BoundTcpPort::bind_many(ifaces.clone(), any(32768), false),
        Err(BindError::InUse)
    ));
    let available = ifaces[0]
        .bind_tcp(BindPortConfig::new(any(32768), false))
        .unwrap();
    drop(available);
    let ports = BoundTcpPort::bind_many(ifaces.clone(), any(0), false).unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].port(), ports[1].port());
    assert_ne!(ports[0].port(), 32768);
    let chosen = ports[0].port();
    drop(ports);
    for iface in &ifaces {
        drop(
            iface
                .bind_tcp(BindPortConfig::new(any(chosen), false))
                .unwrap(),
        );
    }
    drop(blocked);
}

#[ktest]
fn multi_iface_listen_failure_retains_all_ports_without_partial_publication() {
    let ifaces = ifaces();
    let single = ifaces[1]
        .bind_tcp(BindPortConfig::new(any(33000), true))
        .unwrap();
    let listener = TcpListener::new_listen(single, 4, &options(), ())
        .unwrap_or_else(|_| panic!("single listener failed"));
    let ports = BoundTcpPort::bind_many(ifaces.clone(), any(33000), true).unwrap();
    let returned = match TcpListener::new_listen_many(ports, 4, &options(), ()) {
        Err((ports, ListenError::AddressInUse)) => ports,
        _ => panic!("conflicting listen did not return all bindings"),
    };
    assert_eq!(returned.len(), 2);
    let first = ifaces[0]
        .bind_tcp(BindPortConfig::new(any(33000), true))
        .unwrap();
    let first_listener = TcpListener::new_listen(first, 4, &options(), ())
        .unwrap_or_else(|_| panic!("failed group published a partial listener"));
    first_listener.close();
    listener.close();
    ifaces.iter().for_each(|iface| iface.poll());
    drop(first_listener);
    drop(listener);
    let listeners = TcpListener::new_listen_many(returned, 4, &options(), ())
        .unwrap_or_else(|_| panic!("returned bindings could not listen"));
    listeners.iter().for_each(|listener| listener.close());
    ifaces.iter().for_each(|iface| iface.poll());
}

fn connect(iface: &Arc<dyn Iface<TestExt>>, port: u16) -> TcpConnection<TestExt> {
    let address = iface.ipv4_cidr().unwrap().address();
    let bound = iface
        .bind_tcp(BindPortConfig::new(
            IpEndpoint::new(address.into(), 0),
            false,
        ))
        .unwrap();
    let connection =
        TcpConnection::new_connect(bound, IpEndpoint::new(address.into(), port), &options(), ())
            .unwrap_or_else(|_| panic!("connection setup failed"));
    iface.poll();
    connection
}

#[ktest]
fn multi_iface_listener_has_one_backlog_capacity_and_option_set() {
    let ifaces = ifaces();
    let ports = BoundTcpPort::bind_many(ifaces.clone(), any(0), false).unwrap();
    let port = ports[0].port();
    let listeners = TcpListener::new_listen_many(ports, 2, &options(), ())
        .unwrap_or_else(|_| panic!("group listen failed"));
    listeners[1].set_nagle_enabled(false);
    let first = connect(&ifaces[0], port);
    let second = connect(&ifaces[0], port);
    assert!(matches!(first.connect_state(), ConnectState::Connected));
    assert!(matches!(second.connect_state(), ConnectState::Connected));
    let overflow = connect(&ifaces[1], port);
    assert!(matches!(overflow.connect_state(), ConnectState::Connecting));
    let (accepted_first, _) = listeners[1].accept().unwrap();
    let (accepted_second, _) = listeners[1].accept().unwrap();
    assert!(Arc::ptr_eq(accepted_first.iface(), &ifaces[0]));
    assert!(!accepted_first.raw_with(|socket| socket.nagle_enabled()));
    assert!(listeners[0].accept().is_none());
    for connection in [
        &first,
        &second,
        &overflow,
        &accepted_first,
        &accepted_second,
    ] {
        connection.reset();
    }
    listeners.iter().for_each(|listener| listener.close());
    ifaces.iter().for_each(|iface| iface.poll());
    drop(listeners);
    drop((first, second, overflow, accepted_first, accepted_second));
    for iface in &ifaces {
        drop(
            iface
                .bind_tcp(BindPortConfig::new(any(port), false))
                .unwrap(),
        );
    }
}

#[ktest]
fn multi_iface_close_releases_pending_connections_on_every_interface() {
    let ifaces = ifaces();
    let ports = BoundTcpPort::bind_many(ifaces.clone(), any(0), false).unwrap();
    let port = ports[0].port();
    let listeners = TcpListener::new_listen_many(ports, 4, &options(), ())
        .unwrap_or_else(|_| panic!("group listen failed"));
    let first = connect(&ifaces[0], port);
    let second = connect(&ifaces[1], port);
    assert!(matches!(first.connect_state(), ConnectState::Connected));
    assert!(matches!(second.connect_state(), ConnectState::Connected));
    listeners.iter().for_each(|listener| listener.close());
    ifaces.iter().for_each(|iface| iface.poll());
    assert!(!first.raw_with(|socket| socket.is_open()));
    assert!(!second.raw_with(|socket| socket.is_open()));
    drop(listeners);
    drop((first, second));
    for iface in &ifaces {
        drop(
            iface
                .bind_tcp(BindPortConfig::new(any(port), false))
                .unwrap(),
        );
    }
}

mod udp {
    use alloc::{boxed::Box, vec};

    use super::*;
    use crate::{
        errors::udp::{RecvError, SendError},
        iface::BoundUdpPort,
        socket::{ReceiveBehavior, UdpSocket},
    };

    fn socket(ifaces: &[Arc<dyn Iface<TestExt>>]) -> UdpSocket<TestExt> {
        let ports = BoundUdpPort::bind_many(ifaces.to_vec(), any(0), false).unwrap();
        UdpSocket::new_bind(ports, ()).unwrap_or_else(|_| panic!("UDP bind failed"))
    }

    fn destination(socket: &UdpSocket<TestExt>, iface: &Arc<dyn Iface<TestExt>>) -> IpEndpoint {
        IpEndpoint::new(
            iface.ipv4_cidr().unwrap().address().into(),
            socket.local_endpoint().port,
        )
    }

    fn payload(size: usize, byte: u8) -> Box<[u8]> {
        vec![byte; size].into_boxed_slice()
    }

    #[ktest]
    fn udp_common_port_rolls_back_without_conflicting_with_tcp() {
        let ifaces = ifaces();
        let blocked = ifaces[1]
            .bind_udp(BindPortConfig::new(any(32768), false))
            .unwrap();
        assert!(matches!(
            BoundUdpPort::bind_many(ifaces.clone(), any(32768), false),
            Err(BindError::InUse)
        ));
        let available = ifaces[0]
            .bind_udp(BindPortConfig::new(any(32768), false))
            .unwrap();
        let tcp = BoundTcpPort::bind_many(ifaces.clone(), any(32768), false).unwrap();
        let ports = BoundUdpPort::bind_many(ifaces.clone(), any(0), false).unwrap();
        assert_ne!(ports[0].port(), 32768);
        assert_eq!(ports[0].port(), ports[1].port());
        drop((blocked, available, ports, tcp));
        drop(BoundUdpPort::bind_many(ifaces, any(32768), false).unwrap());
    }

    #[ktest]
    fn udp_payload_capacity_is_shared_without_blocking_another_interface() {
        let ifaces = ifaces();
        let sender = socket(&ifaces);
        let receiver = socket(&ifaces);
        for (index, iface) in ifaces.iter().enumerate() {
            sender
                .send(
                    iface,
                    destination(&receiver, iface),
                    payload(32768, index as u8),
                )
                .unwrap();
        }
        assert!(!sender.can_send());
        assert_eq!(
            sender.send(
                &ifaces[0],
                destination(&receiver, &ifaces[0]),
                payload(1, 9)
            ),
            Err(SendError::BufferFull)
        );
        ifaces[1].poll();
        receiver
            .recv(ReceiveBehavior::Recv, |data, meta| {
                assert_eq!(data.len(), 32768);
                assert!(data.iter().all(|byte| *byte == 1));
                assert_eq!(meta.endpoint, destination(&sender, &ifaces[1]));
            })
            .unwrap();
        assert!(sender.can_send());
        assert!(matches!(
            receiver.recv(ReceiveBehavior::Peek, |_, _| ()),
            Err(RecvError::Exhausted)
        ));
        ifaces[0].poll();
        receiver
            .recv(ReceiveBehavior::Peek, |data, _| {
                assert!(data.iter().all(|byte| *byte == 0))
            })
            .unwrap();
        receiver
            .recv(ReceiveBehavior::Recv, |data, _| {
                assert_eq!(data.len(), 32768)
            })
            .unwrap();
    }

    #[ktest]
    fn udp_empty_datagrams_share_one_packet_limit_and_release_ports_on_close() {
        let ifaces = ifaces();
        let socket = socket(&ifaces);
        let port = socket.local_endpoint().port;
        for index in 0..256 {
            let iface = &ifaces[index % ifaces.len()];
            socket
                .send(iface, destination(&socket, iface), payload(0, 0))
                .unwrap();
        }
        assert!(!socket.can_send());
        assert_eq!(
            socket.send(&ifaces[0], destination(&socket, &ifaces[0]), payload(0, 0)),
            Err(SendError::BufferFull)
        );
        ifaces[0].poll();
        assert!(socket.can_send());
        for _ in 0..128 {
            socket
                .recv(ReceiveBehavior::Recv, |data, _| assert!(data.is_empty()))
                .unwrap();
        }
        assert!(!socket.can_recv());
        drop(socket);
        ifaces[1].poll();
        drop(BoundUdpPort::bind_many(ifaces, any(port), false).unwrap());
    }

    #[ktest]
    fn udp_connect_filters_new_packets_and_keeps_the_existing_receive_queue() {
        let ifaces = ifaces();
        let receiver = socket(&ifaces);
        let peer = socket(&ifaces);
        let other = socket(&ifaces);
        let target = destination(&receiver, &ifaces[0]);
        other.send(&ifaces[0], target, payload(1, 0)).unwrap();
        ifaces[0].poll();
        let remote = destination(&peer, &ifaces[0]);
        receiver.connect(remote, target.addr).unwrap();
        assert_eq!(receiver.local_endpoint().addr, target.addr);
        receiver
            .recv(ReceiveBehavior::Recv, |data, _| assert_eq!(data, &[0]))
            .unwrap();
        other.send(&ifaces[0], target, payload(1, 1)).unwrap();
        peer.send(
            &ifaces[1],
            destination(&receiver, &ifaces[1]),
            payload(1, 2),
        )
        .unwrap();
        peer.send(&ifaces[0], target, payload(1, 3)).unwrap();
        ifaces.iter().for_each(|iface| iface.poll());
        receiver
            .recv(ReceiveBehavior::Peek, |data, meta| {
                assert_eq!(data, &[3]);
                assert_eq!(meta.endpoint, remote);
            })
            .unwrap();
        receiver
            .recv(ReceiveBehavior::Recv, |data, _| assert_eq!(data, &[3]))
            .unwrap();
        assert!(!receiver.can_recv());
    }

    #[ktest]
    fn udp_arrival_metadata_survives_peek_and_later_packets() {
        let ifaces = ifaces();
        let sender = socket(&ifaces);
        let receiver = socket(&ifaces);
        let target = destination(&receiver, &ifaces[1]);
        sender.send(&ifaces[1], target, payload(1, 7)).unwrap();
        ifaces[1].poll();
        let arrival = receiver
            .recv(ReceiveBehavior::Peek, |data, meta| {
                assert_eq!(data, &[7]);
                assert_eq!(meta.destination, target);
                assert_eq!(meta.endpoint, destination(&sender, &ifaces[1]));
                assert!(!meta.timestamp.is_zero());
                meta.timestamp
            })
            .unwrap();
        sender.send(&ifaces[1], target, payload(0, 0)).unwrap();
        ifaces[1].poll();
        receiver
            .recv(ReceiveBehavior::Recv, |data, meta| {
                assert_eq!(data, &[7]);
                assert_eq!(meta.timestamp, arrival);
            })
            .unwrap();
        receiver
            .recv(ReceiveBehavior::Recv, |data, meta| {
                assert!(data.is_empty());
                assert!(meta.timestamp > arrival);
            })
            .unwrap();
        assert!(!receiver.can_recv());
    }

    #[ktest]
    fn udp_reuse_prefers_connected_then_specific_then_the_latest_binding() {
        let ifaces = ifaces();
        let first_ports = BoundUdpPort::bind_many(ifaces.clone(), any(0), true).unwrap();
        let endpoint = first_ports[0].endpoint();
        let first =
            UdpSocket::new_bind(first_ports, ()).unwrap_or_else(|_| panic!("first bind failed"));
        let bind = || {
            UdpSocket::new_bind(
                BoundUdpPort::bind_many(ifaces.clone(), endpoint, true).unwrap(),
                (),
            )
            .unwrap_or_else(|_| panic!("reusable bind failed"))
        };
        let second = bind();
        let third = bind();
        drop(first);
        let peer = socket(&ifaces);
        let target = destination(&second, &ifaces[0]);
        peer.send(&ifaces[0], target, payload(1, 1)).unwrap();
        ifaces[0].poll();
        assert!(third.can_recv());
        assert!(!second.can_recv());
        third.recv(ReceiveBehavior::Recv, |_, _| ()).unwrap();
        let specific_port = ifaces[0]
            .bind_udp(BindPortConfig::new(target, true))
            .unwrap();
        let specific = UdpSocket::new_bind(vec![specific_port], ())
            .unwrap_or_else(|_| panic!("specific bind failed"));
        peer.send(&ifaces[0], target, payload(1, 2)).unwrap();
        ifaces[0].poll();
        assert!(specific.can_recv());
        assert!(!third.can_recv());
        specific.recv(ReceiveBehavior::Recv, |_, _| ()).unwrap();
        second
            .connect(destination(&peer, &ifaces[0]), target.addr)
            .unwrap();
        peer.send(&ifaces[0], target, payload(1, 3)).unwrap();
        ifaces[0].poll();
        assert!(second.can_recv());
        assert!(!specific.can_recv());
        second
            .recv(ReceiveBehavior::Recv, |data, _| assert_eq!(data, &[3]))
            .unwrap();
    }
}
