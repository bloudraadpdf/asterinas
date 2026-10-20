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

fn listen_many(
    ifaces: &[Arc<dyn Iface<TestExt>>],
    backlog: usize,
) -> (u16, Vec<TcpListener<TestExt>>) {
    let ports = BoundTcpPort::bind_many(ifaces.to_vec(), any(0), false).unwrap();
    let port = ports[0].port();
    let listeners = TcpListener::new_listen_many(ports, backlog, &options(), ())
        .unwrap_or_else(|_| panic!("group listen failed"));
    (port, listeners)
}

#[ktest]
fn multi_iface_listener_remembers_receive_interface_after_accept() {
    let ifaces = ifaces();
    let (port, listeners) = listen_many(&ifaces, 4);
    assert!(
        listeners
            .iter()
            .all(|listener| listener.last_receive_iface().is_none())
    );
    let first = connect(&ifaces[1], port);
    assert!(
        listeners
            .iter()
            .all(|listener| { listener.last_receive_iface() == Some(ifaces[1].index()) })
    );
    let (accepted_first, _) = listeners[0].accept().unwrap();
    assert!(listeners[0].accept().is_none());
    assert_eq!(listeners[0].last_receive_iface(), Some(ifaces[1].index()));
    let second = connect(&ifaces[0], port);
    assert!(
        listeners
            .iter()
            .all(|listener| { listener.last_receive_iface() == Some(ifaces[0].index()) })
    );
    let (accepted_second, _) = listeners[1].accept().unwrap();
    for connection in [&first, &second, &accepted_first, &accepted_second] {
        connection.reset();
    }
    listeners.iter().for_each(|listener| listener.close());
    ifaces.iter().for_each(|iface| iface.poll());
}

#[ktest]
fn multi_iface_close_releases_pending_connections_on_every_interface() {
    let ifaces = ifaces();
    let (port, listeners) = listen_many(&ifaces, 4);
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
        assert!(receiver.busy_poll_iface().is_none());
        sender.send(&ifaces[1], target, payload(1, 7)).unwrap();
        ifaces[1].poll();
        assert_eq!(
            receiver.busy_poll_iface().unwrap().index(),
            ifaces[1].index()
        );
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
        sender
            .send(
                &ifaces[0],
                destination(&receiver, &ifaces[0]),
                payload(1, 9),
            )
            .unwrap();
        ifaces[0].poll();
        assert_eq!(
            receiver.busy_poll_iface().unwrap().index(),
            ifaces[0].index()
        );
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

mod ingress_budget {
    use alloc::{collections::VecDeque, vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        device::{DeviceCapabilities, NetError},
        iface::BoundUdpPort,
        packet::{FreshTxPacket, LinkLayer, RxPacket, TxPacket},
        socket::{ReceiveBehavior, UdpSocket},
    };

    #[derive(Debug)]
    struct Device {
        loopback: Loopback,
        packets: VecDeque<Vec<u8>>,
        received: usize,
        sent: usize,
        egress_checks: AtomicUsize,
        notifications: usize,
    }

    impl AnyNetworkDevice for Device {
        fn mac_addr(&self) -> crate::device::EthernetAddress {
            self.loopback.mac_addr()
        }
        fn capabilities(&self) -> DeviceCapabilities {
            self.loopback.capabilities()
        }
        fn can_receive(&self) -> bool {
            !self.packets.is_empty()
        }
        fn can_send(&self) -> bool {
            self.egress_checks.fetch_add(1, Ordering::Relaxed);
            true
        }
        fn receive(&mut self) -> Result<RxPacket<LinkLayer>, NetError> {
            let Some(bytes) = self.packets.pop_front() else {
                return Err(NetError::NotReady);
            };
            let mut builder = FreshTxPacket::<LinkLayer>::alloc(bytes.len())
                .map_err(|_| NetError::NoMemory)?
                .to_builder();
            let written = builder
                .append_writer()
                .write(&mut ostd::mm::VmReader::from(bytes.as_slice()));
            builder.commit(written);
            self.received += 1;
            Ok(builder.build().to_rx_packet().unwrap())
        }
        fn send(&mut self, packet: TxPacket<LinkLayer>) -> Result<(), NetError> {
            self.sent += 1;
            self.loopback.send(packet)
        }
        fn alloc_tx_buffer(payload_len: usize) -> Result<FreshTxPacket, ostd::Error> {
            Loopback::alloc_tx_buffer(payload_len)
        }
        fn free_processed_tx_buffers(&mut self) {}
        fn notify_poll_end(&mut self) {
            self.notifications += 1;
        }
    }

    struct Driver(Arc<SpinLock<Device, BottomHalfDisabled>>);
    impl WithDevice for Driver {
        type Device = Device;
        fn with<F, R>(&self, operation: F) -> R
        where
            F: FnOnce(&mut dyn AnyNetworkDevice) -> R,
        {
            operation(&mut *self.0.lock())
        }
    }

    fn device_iface() -> (
        Arc<dyn Iface<TestExt>>,
        Arc<SpinLock<Device, BottomHalfDisabled>>,
    ) {
        let device = Arc::new(SpinLock::new(Device {
            loopback: Loopback::new(),
            packets: VecDeque::new(),
            received: 0,
            sent: 0,
            egress_checks: AtomicUsize::new(0),
            notifications: 0,
        }));
        let iface = IpIface::new(
            Driver(device.clone()),
            Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 1), 24),
            None,
            InterfaceName::from_str_truncated("budget"),
            NoScheduler,
            InterfaceType::LOOPBACK,
            InterfaceFlags::UP | InterfaceFlags::LOOPBACK,
        );
        (iface, device)
    }

    #[ktest]
    fn ingress_budget_limits_packets_including_discarded_input() {
        let (iface, device) = device_iface();
        device.lock().packets.extend([vec![0], vec![0], vec![0]]);
        assert_eq!(iface.poll_with_budget(0), 0);
        assert_eq!(device.lock().notifications, 0);
        assert_eq!(iface.poll_with_budget(1), 1);
        assert_eq!(device.lock().packets.len(), 2);
        assert_eq!(iface.poll_with_budget(1), 1);
        assert_eq!(iface.poll_with_budget(8), 1);
        assert_eq!(iface.poll_with_budget(8), 0);
        let device = device.lock();
        assert_eq!(device.received, 3);
        assert_eq!(device.egress_checks.load(Ordering::Relaxed), 0);
        assert_eq!(device.sent, 0);
        assert_eq!(device.notifications, 4);
    }

    fn udp_packet(port: u16, value: u8) -> Vec<u8> {
        let mut bytes = vec![
            0x45, 0, 0, 29, 0, 0, 0, 0, 64, 17, 0, 0, 192, 0, 2, 2, 192, 0, 2, 1, 0xac, 0x00, 0, 0,
            0, 9, 0, 0, value,
        ];
        bytes[22..24].copy_from_slice(&port.to_be_bytes());
        bytes
    }

    #[ktest]
    fn busy_poll_delivers_device_udp_but_leaves_general_loopback_egress_pending() {
        let (iface, device) = device_iface();
        let ports = BoundUdpPort::bind_many(vec![iface.clone()], any(0), false).unwrap();
        let receiver =
            UdpSocket::new_bind(ports, ()).unwrap_or_else(|_| panic!("UDP receiver bind failed"));
        let target = IpEndpoint::new(
            iface.ipv4_cidr().unwrap().address().into(),
            receiver.local_endpoint().port,
        );
        device
            .lock()
            .packets
            .extend([udp_packet(target.port, 7), udp_packet(target.port, 9)]);
        assert_eq!(iface.poll_with_budget(1), 1);
        receiver
            .recv(ReceiveBehavior::Recv, |bytes, _| assert_eq!(bytes, &[7]))
            .unwrap();
        assert!(!receiver.can_recv());
        assert_eq!(device.lock().packets.len(), 1);
        assert_eq!(receiver.busy_poll_iface().unwrap().index(), iface.index());
        assert_eq!(iface.poll_with_budget(1), 1);
        receiver
            .recv(ReceiveBehavior::Recv, |bytes, _| assert_eq!(bytes, &[9]))
            .unwrap();
        receiver
            .send(&iface, target, vec![11].into_boxed_slice())
            .unwrap();
        assert_eq!(iface.poll_with_budget(1), 0);
        assert!(!receiver.can_recv());
        assert_eq!(device.lock().egress_checks.load(Ordering::Relaxed), 0);
        iface.poll();
        receiver
            .recv(ReceiveBehavior::Recv, |bytes, _| assert_eq!(bytes, &[11]))
            .unwrap();
    }

    #[ktest]
    fn exhausted_local_tcp_allowance_retains_reply_for_the_next_poll() {
        let (iface, device) = device_iface();
        let bound = iface.bind_tcp(BindPortConfig::new(any(0), false)).unwrap();
        let port = bound.port();
        let listener = TcpListener::new_listen(bound, 4, &options(), ())
            .unwrap_or_else(|_| panic!("TCP listener bind failed"));
        let mut syn = vec![
            0x45, 0, 0, 40, 0, 0, 0, 0, 64, 6, 0, 0, 192, 0, 2, 1, 192, 0, 2, 1, 0xac, 0, 0, 0, 0,
            0, 0, 1, 0, 0, 0, 0, 0x50, 2, 0xff, 0xff, 0, 0, 0, 0,
        ];
        syn[22..24].copy_from_slice(&port.to_be_bytes());
        device.lock().packets.push_back(syn);
        {
            let mut device = device.lock();
            let mut interface = iface.common().interface();
            let mut sockets = iface.common().sockets();
            let mut actions = Vec::new();
            // IpIface implements PollPhy; use a local stateless adapter to process IP framing.
            let phy = TestIpPhy;
            let reply = {
                let mut context = super::super::poll::PollContext::new(
                    interface.as_mut(),
                    &sockets,
                    &mut actions,
                );
                context.limit_local_tcp(1);
                assert_eq!(context.poll_ingress(&mut *device, &phy, 1), 1);
                assert_eq!(context.remaining_local_tcp_steps(), 0);
                context
                    .take_deferred_tcp()
                    .expect("local SYN-ACK must be retained")
            };
            for action in actions {
                match action {
                    super::super::poll::SocketTableAction::AddTcpConn(connection) => {
                        assert!(sockets.insert_connection(connection).is_ok());
                    }
                    super::super::poll::SocketTableAction::DelTcpConn(key) => {
                        sockets.remove_dead_tcp_connection(&key);
                    }
                }
            }
            interface.defer_tcp(reply);
            assert_eq!(device.sent, 0);
        }
        assert_eq!(iface.poll_with_budget(1), 0);
        assert!(iface.common().interface().take_deferred_tcp().is_none());
        assert_eq!(device.lock().sent, 0);
        listener.close();
        iface.poll();
    }

    struct TestIpPhy;
    impl super::super::common::PollPhy for TestIpPhy {
        fn process(
            &self,
            packet: RxPacket<LinkLayer>,
            _: &mut smoltcp::iface::Context,
        ) -> Option<super::super::common::PhyProcessResult> {
            Some(super::super::common::PhyProcessResult::Ip(packet.peel(0)))
        }
        fn dispatch(
            &self,
            packet: super::super::common::TxPacketWithDst,
            _: &mut smoltcp::iface::Context,
        ) -> Option<TxPacket<LinkLayer>> {
            Some(packet.packet.pack(0))
        }
        fn alloc_tx_buffer(&self, payload_len: usize) -> Result<FreshTxPacket, ostd::Error> {
            Loopback::alloc_tx_buffer(payload_len)
        }
    }
}
