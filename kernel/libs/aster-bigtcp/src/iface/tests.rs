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

pub(super) struct TestExt;
pub(super) struct NoScheduler;
struct TestDevice(SpinLock<Loopback, BottomHalfDisabled>);

impl Ext for TestExt {
    type ScheduleNextPoll = NoScheduler;
    type TcpEventObserver = ();
    type UdpEventObserver = ();
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
