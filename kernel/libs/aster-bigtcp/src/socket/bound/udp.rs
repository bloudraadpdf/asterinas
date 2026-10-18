// SPDX-License-Identifier: MPL-2.0

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aster_softirq::BottomHalfDisabled;
use ostd::sync::SpinLock;
use smoltcp::{
    iface::Context,
    socket::udp::{BindError, UdpMetadata},
    wire::{IpAddress, IpEndpoint, IpProtocol, IpRepr, UdpRepr},
};

use super::ReceiveBehavior;
use crate::{
    errors::udp::{RecvError, SendError},
    ext::Ext,
    iface::{BoundUdpPort, Iface, IsUnicast, PacketSlice},
    socket::{
        RawUdpSocket, UDP_SEND_PAYLOAD_LEN,
        event::{SocketEventObserver, SocketEvents},
        unbound::{UDP_METADATA_LEN, new_udp_socket},
    },
};

/// A UDP socket with shared receive and transmit capacity across its interfaces.
pub struct UdpSocket<E: Ext> {
    first: Arc<UdpSocketBg<E>>,
    others: Vec<Arc<UdpSocketBg<E>>>,
}

struct Shared<E: Ext> {
    binding_id: u64,
    state: SpinLock<State, BottomHalfDisabled>,
    observer: E::UdpEventObserver,
}

static NEXT_BINDING_ID: AtomicU64 = AtomicU64::new(1);

struct State {
    socket: Box<RawUdpSocket>,
    outgoing: BTreeMap<u32, VecDeque<Datagram>>,
    outgoing_bytes: usize,
    outgoing_packets: usize,
    connected: Option<(IpEndpoint, IpAddress)>,
    closed: bool,
}

struct Datagram {
    destination: IpEndpoint,
    source: IpAddress,
    payload: Box<[u8]>,
}

impl State {
    fn accepts_peer(&self, ip: &IpRepr, udp: &UdpRepr) -> bool {
        !self.closed
            && self.connected.is_none_or(|(peer, local)| {
                peer.addr == ip.src_addr() && peer.port == udp.src_port && local == ip.dst_addr()
            })
    }
}

pub(crate) struct UdpSocketBg<E: Ext> {
    bound: BoundUdpPort<E>,
    shared: Arc<Shared<E>>,
    need_dispatch: AtomicBool,
}

impl<E: Ext> UdpSocketBg<E> {
    pub(crate) fn can_process(&self, dst_port: u16) -> bool {
        self.bound.port() == dst_port
    }

    fn accepts_local(&self, cx: &Context, ip: &IpRepr, udp: &UdpRepr) -> bool {
        let destination = ip.dst_addr();
        self.bound.addr().version() == destination.version()
            && self.can_process(udp.dst_port)
            && if self.bound.addr().is_unspecified() {
                cx.is_unicast_local(destination) || cx.is_broadcast(&destination)
            } else {
                destination == *self.bound.addr()
            }
    }

    pub(crate) fn match_score(
        &self,
        cx: &Context,
        ip: &IpRepr,
        udp: &UdpRepr,
    ) -> Option<(u8, u64)> {
        if !self.accepts_local(cx, ip, udp) {
            return None;
        }
        let state = self.shared.state.lock();
        state.accepts_peer(ip, udp).then(|| {
            (
                if state.connected.is_some() {
                    3
                } else {
                    u8::from(!self.bound.addr().is_unspecified())
                },
                self.shared.binding_id,
            )
        })
    }

    /// Processes a datagram only if its local address and connected peer match.
    pub(crate) fn process(
        &self,
        cx: &mut Context,
        ip_repr: &IpRepr,
        udp_repr: &UdpRepr,
        udp_payload: PacketSlice<'_>,
    ) -> bool {
        if !self.accepts_local(cx, ip_repr, udp_repr) {
            return false;
        }
        let mut state = self.shared.state.lock();
        if !state.accepts_peer(ip_repr, udp_repr) {
            return false;
        }
        state.socket.process(
            cx,
            smoltcp::phy::PacketMeta::default(),
            ip_repr,
            udp_repr,
            udp_payload,
        );
        drop(state);
        self.shared.observer.on_events(SocketEvents::CAN_RECV);
        true
    }

    pub(crate) fn dispatch<D>(&self, cx: &mut Context, dispatch: D)
    where
        D: FnOnce(&mut Context, &IpRepr, &UdpRepr, &[u8]),
    {
        let packet = {
            let mut state = self.shared.state.lock();
            let Some(queue) = state.outgoing.get_mut(&self.bound.iface().index()) else {
                self.need_dispatch.store(false, Ordering::Relaxed);
                return;
            };
            let packet = queue.pop_front();
            self.need_dispatch
                .store(!queue.is_empty(), Ordering::Relaxed);
            let Some(packet) = packet else {
                return;
            };
            state.outgoing_bytes -= packet.payload.len();
            state.outgoing_packets -= 1;
            packet
        };
        self.shared.observer.on_events(SocketEvents::CAN_SEND);
        let udp_repr = UdpRepr {
            src_port: self.bound.port(),
            dst_port: packet.destination.port,
        };
        let ip_repr = IpRepr::new(
            packet.source,
            packet.destination.addr,
            IpProtocol::Udp,
            udp_repr.header_len() + packet.payload.len(),
            64,
        );
        dispatch(cx, &ip_repr, &udp_repr, &packet.payload);
    }

    pub(crate) fn need_dispatch(&self) -> bool {
        self.need_dispatch.load(Ordering::Relaxed)
    }
}

impl<E: Ext> UdpSocket<E> {
    /// Binds one socket to all supplied reservations, with one shared capacity limit.
    pub fn new_bind(
        mut bounds: Vec<BoundUdpPort<E>>,
        observer: E::UdpEventObserver,
    ) -> Result<Self, (Vec<BoundUdpPort<E>>, BindError)> {
        let Some(first) = bounds.first() else {
            return Err((bounds, BindError::Unaddressable));
        };
        let endpoint = first.endpoint();
        bounds.sort_unstable_by_key(|bound| bound.iface().index());
        if bounds.iter().any(|bound| bound.endpoint() != endpoint)
            || bounds
                .windows(2)
                .any(|pair| pair[0].iface().index() == pair[1].iface().index())
        {
            return Err((bounds, BindError::InvalidState));
        }
        let Ok(binding_id) =
            NEXT_BINDING_ID
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        else {
            return Err((bounds, BindError::InvalidState));
        };
        let mut socket = new_udp_socket();
        let local_endpoint = {
            let interface = bounds[0].iface().common().interface();
            IpEndpoint::new(
                interface.map_broadcast_to_local(endpoint.addr),
                endpoint.port,
            )
        };
        if let Err(error) = socket.bind(local_endpoint) {
            return Err((bounds, error));
        }
        let shared = Arc::new(Shared {
            binding_id,
            state: SpinLock::new(State {
                socket,
                outgoing: BTreeMap::new(),
                outgoing_bytes: 0,
                outgoing_packets: 0,
                connected: None,
                closed: false,
            }),
            observer,
        });
        let mut sockets = bounds.into_iter().map(|bound| {
            Arc::new(UdpSocketBg {
                bound,
                shared: shared.clone(),
                need_dispatch: AtomicBool::new(false),
            })
        });
        let first = sockets.next().unwrap();
        let others = sockets.collect();
        let socket = Self { first, others };
        let mut tables: Vec<_> = socket
            .sockets()
            .map(|bg| bg.bound.iface().common().sockets())
            .collect();
        for (table, bg) in tables.iter_mut().zip(socket.sockets()) {
            table.insert_udp_socket(bg.clone());
        }
        drop(tables);
        Ok(socket)
    }

    fn sockets(&self) -> impl Iterator<Item = &Arc<UdpSocketBg<E>>> {
        core::iter::once(&self.first).chain(self.others.iter())
    }

    pub fn bound_ports(&self) -> impl Iterator<Item = &BoundUdpPort<E>> {
        self.sockets().map(|socket| &socket.bound)
    }

    pub fn local_endpoint(&self) -> IpEndpoint {
        let mut endpoint = self.first.bound.endpoint();
        if let Some((_, source)) = self.first.shared.state.lock().connected {
            endpoint.addr = source;
        }
        endpoint
    }

    /// Filters newly received packets without removing previously queued datagrams.
    pub fn connect(&self, peer: IpEndpoint, source: IpAddress) -> Result<(), BindError> {
        if peer.addr.version() != source.version()
            || source.version() != self.first.bound.addr().version()
        {
            return Err(BindError::Unaddressable);
        }
        self.first.shared.state.lock().connected = Some((peer, source));
        Ok(())
    }

    /// Queues a complete datagram. The payload is not copied again.
    ///
    /// The caller must poll `iface` after success.
    pub fn send(
        &self,
        iface: &Arc<dyn Iface<E>>,
        destination: IpEndpoint,
        payload: Box<[u8]>,
    ) -> Result<(), SendError> {
        let Some(socket) = self
            .sockets()
            .find(|bg| Arc::ptr_eq(bg.bound.iface(), iface))
        else {
            return Err(SendError::Unaddressable);
        };
        if destination.port == 0
            || destination.addr.is_unspecified()
            || destination.addr.version() != socket.bound.addr().version()
        {
            return Err(SendError::Unaddressable);
        }
        if payload.len() > Self::max_payload_len(destination.addr) {
            return Err(SendError::TooLarge);
        }
        let source = match destination.addr {
            IpAddress::Ipv4(_) => iface.ipv4_cidr().map(|cidr| cidr.address().into()),
            IpAddress::Ipv6(_) => iface.ipv6_cidr().map(|cidr| cidr.address().into()),
        }
        .ok_or(SendError::Unaddressable)?;
        let source = if socket.bound.addr().is_unspecified() {
            source
        } else {
            *socket.bound.addr()
        };
        let interface = iface.common().interface();
        let mut state = self.first.shared.state.lock();
        if state.outgoing_packets >= UDP_METADATA_LEN
            || payload.len() > UDP_SEND_PAYLOAD_LEN - state.outgoing_bytes
        {
            return Err(SendError::BufferFull);
        }
        let source =
            interface.map_broadcast_to_local(state.connected.map_or(source, |(_, local)| local));
        state.outgoing_bytes += payload.len();
        state.outgoing_packets += 1;
        state
            .outgoing
            .entry(iface.index())
            .or_default()
            .push_back(Datagram {
                destination,
                source,
                payload,
            });
        socket.need_dispatch.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn max_payload_len(destination: IpAddress) -> usize {
        match destination {
            IpAddress::Ipv4(_) => 65507,
            IpAddress::Ipv6(_) => 65527,
        }
    }

    pub fn recv<F, R>(&self, behavior: ReceiveBehavior, copy: F) -> Result<R, RecvError>
    where
        F: FnOnce(&[u8], UdpMetadata) -> R,
    {
        let mut state = self.first.shared.state.lock();
        let (data, meta) = match behavior {
            ReceiveBehavior::Recv => state.socket.recv()?,
            ReceiveBehavior::Peek => {
                let (data, meta) = state.socket.peek()?;
                (data, *meta)
            }
        };
        Ok(copy(data, meta))
    }

    pub fn can_recv(&self) -> bool {
        self.first.shared.state.lock().socket.can_recv()
    }

    pub fn can_send(&self) -> bool {
        let state = self.first.shared.state.lock();
        state.outgoing_packets < UDP_METADATA_LEN && state.outgoing_bytes < UDP_SEND_PAYLOAD_LEN
    }
}

impl<E: Ext> Drop for UdpSocket<E> {
    fn drop(&mut self) {
        {
            let mut state = self.first.shared.state.lock();
            state.closed = true;
            state.socket.close();
            state.outgoing.clear();
            state.outgoing_bytes = 0;
            state.outgoing_packets = 0;
            self.sockets()
                .for_each(|socket| socket.need_dispatch.store(false, Ordering::Relaxed));
        }
        for socket in self.sockets() {
            socket.bound.iface().common().remove_udp_socket(socket);
        }
    }
}
