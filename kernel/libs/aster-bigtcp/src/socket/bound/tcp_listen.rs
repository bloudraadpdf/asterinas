// SPDX-License-Identifier: MPL-2.0

use alloc::{boxed::Box, collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::num::NonZeroU8;

use aster_softirq::BottomHalfDisabled;
use ostd::sync::SpinLock;
use smoltcp::{
    socket::PollAt,
    storage::SliceLike,
    time::Duration,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpRepr, TcpRepr},
};

use super::{
    common::{Inner, NeedIfacePoll, Socket, SocketBg},
    fast_open::FastOpenQueue,
    tcp_conn::{TcpConnection, TcpConnectionBg, TcpConnectionInner, TcpProcessResult},
};
use crate::{
    errors::tcp::ListenError,
    ext::Ext,
    iface::{BindPortConfig, BoundTcpPort, PacketSlice, PollableIfaceMut},
    socket::{
        option::{RawTcpOption, RawTcpSetOption},
        unbound::{RawTcpSocket, new_tcp_socket},
    },
    socket_table::{ConnectionKey, ListenerKey},
};

pub type TcpListener<E> = Socket<TcpListenerInner<E>, E>;

pub(super) struct TcpBacklog<E: Ext> {
    socket: Box<RawTcpSocket>,
    max_conn: usize,
    fast_open: FastOpenQueue,
    pub(super) connecting: BTreeMap<ConnectionKey, TcpConnection<E>>,
    pub(super) connected: Vec<TcpConnection<E>>,
    pub(super) last_receive_iface: Option<u32>,
}

/// States needed by [`TcpListenerBg`].
pub struct TcpListenerInner<E: Ext> {
    pub(super) backlog: Arc<SpinLock<TcpBacklog<E>, BottomHalfDisabled>>,
    listener_key: ListenerKey,
}

impl<E: Ext> TcpListenerInner<E> {
    fn new(
        backlog: Arc<SpinLock<TcpBacklog<E>, BottomHalfDisabled>>,
        listener_key: ListenerKey,
    ) -> Self {
        Self {
            backlog,
            listener_key,
        }
    }
}

impl<E: Ext> Inner<E> for TcpListenerInner<E> {
    type BoundPort = BoundTcpPort<E>;
    type Observer = E::TcpEventObserver;

    fn on_drop(this: &Arc<SocketBg<Self, E>>) {
        debug_assert_eq!(
            Arc::strong_count(this),
            1,
            "a listener must be closed before dropping"
        );
    }
}

pub(crate) type TcpListenerBg<E> = SocketBg<TcpListenerInner<E>, E>;

impl<E: Ext> TcpListener<E> {
    /// Listens at a specified endpoint.
    ///
    /// Polling the iface is _not_ required after this method succeeds.
    pub fn new_listen(
        bound: BoundTcpPort<E>,
        max_conn: usize,
        option: &RawTcpOption,
        observer: E::TcpEventObserver,
    ) -> Result<Self, (BoundTcpPort<E>, ListenError)> {
        let local_endpoint = bound.endpoint();

        let iface = bound.iface().clone();
        let mut sockets = iface.common().sockets();

        let listener_key = ListenerKey::new(local_endpoint.addr, local_endpoint.port);

        if sockets.has_listener_conflict(&listener_key) {
            return Err((bound, ListenError::AddressInUse));
        }

        let backlog = match Self::new_backlog(local_endpoint, max_conn, option) {
            Ok(backlog) => backlog,
            Err(err) => return Err((bound, err)),
        };
        let inner = TcpListenerInner::new(backlog, listener_key);

        let listener = Self::new(bound, inner);
        listener.init_observer(observer);
        let res = sockets.insert_listener(listener.inner().clone());
        debug_assert!(res.is_ok());

        Ok(listener)
    }

    fn new_backlog(
        local_endpoint: IpEndpoint,
        max_conn: usize,
        option: &RawTcpOption,
    ) -> Result<Arc<SpinLock<TcpBacklog<E>, BottomHalfDisabled>>, ListenError> {
        let mut socket = new_tcp_socket();
        option.apply(&mut socket);
        let addr = match local_endpoint.addr {
            IpAddress::Ipv4(address) if address.is_unspecified() => None,
            IpAddress::Ipv6(address) if address.is_unspecified() => None,
            address => Some(address),
        };
        socket.listen(IpListenEndpoint {
            addr,
            port: local_endpoint.port,
        })?;
        Ok(Arc::new(SpinLock::new(TcpBacklog {
            socket,
            max_conn,
            fast_open: FastOpenQueue::new(option.fast_open_backlog),
            connecting: BTreeMap::new(),
            connected: Vec::new(),
            last_receive_iface: None,
        })))
    }

    /// Listens on multiple interfaces with one shared backlog and socket option set.
    /// All bindings must use the same endpoint on distinct interfaces.
    /// Failure returns every binding without publishing a listener.
    pub fn new_listen_many(
        mut bounds: Vec<BoundTcpPort<E>>,
        max_conn: usize,
        option: &RawTcpOption,
        observer: E::TcpEventObserver,
    ) -> Result<Vec<Self>, (Vec<BoundTcpPort<E>>, ListenError)>
    where
        E::TcpEventObserver: Clone,
    {
        let Some(first) = bounds.first() else {
            return Err((bounds, ListenError::Unaddressable));
        };
        let endpoint = first.endpoint();
        if bounds.iter().any(|bound| bound.endpoint() != endpoint) {
            return Err((bounds, ListenError::Unaddressable));
        }
        bounds.sort_unstable_by_key(|bound| bound.iface().index());
        if bounds
            .windows(2)
            .any(|pair| pair[0].iface().index() == pair[1].iface().index())
        {
            return Err((bounds, ListenError::InvalidState));
        }
        let backlog = match Self::new_backlog(endpoint, max_conn, option) {
            Ok(backlog) => backlog,
            Err(err) => return Err((bounds, err)),
        };
        let ifaces: Vec<_> = bounds.iter().map(|bound| bound.iface().clone()).collect();
        // Match the port reservation lock order. No listener is visible until every check passes.
        let mut tables: Vec<_> = ifaces
            .iter()
            .map(|iface| iface.common().sockets())
            .collect();
        let key = ListenerKey::new(endpoint.addr, endpoint.port);
        if tables.iter().any(|table| table.has_listener_conflict(&key)) {
            return Err((bounds, ListenError::AddressInUse));
        }
        let listeners = bounds
            .into_iter()
            .zip(tables.iter_mut())
            .map(|(bound, table)| {
                let listener = Self::new(bound, TcpListenerInner::new(backlog.clone(), key));
                listener.init_observer(observer.clone());
                let result = table.insert_listener(listener.inner().clone());
                debug_assert!(result.is_ok());
                listener
            })
            .collect();
        Ok(listeners)
    }

    /// Accepts a TCP connection.
    ///
    /// Polling the iface is _not_ required after this method succeeds.
    pub fn accept(&self) -> Option<(TcpConnection<E>, IpEndpoint)> {
        let accepted = {
            let mut backlog = self.0.inner.backlog.lock();
            backlog.connected.pop()?
        };

        let remote_endpoint = {
            // The lock on `accepted` cannot be locked after locking `self`, otherwise we might get
            // a deadlock due to inconsistent lock order problems.
            let mut socket = accepted.0.inner.lock();

            socket.listener = None;

            // A queued connection may have been reset, which clears smoltcp's internal endpoint tuple.
            // Use the saved connection key so that it can still be accepted.
            accepted.0.connection_key().remote_endpoint()
        };

        Some((accepted, remote_endpoint))
    }

    /// Returns whether there is a TCP connection to accept.
    ///
    /// It's the caller's responsibility to deal with race conditions when using this method.
    pub fn can_accept(&self) -> bool {
        !self.0.inner.backlog.lock().connected.is_empty()
    }

    /// Returns the interface that last received traffic for this listener group.
    pub fn last_receive_iface(&self) -> Option<u32> {
        self.0.inner.backlog.lock().last_receive_iface
    }

    /// Changes the limit for subsequent authenticated passive opens.
    pub fn set_fast_open_backlog(&self, limit: usize) {
        self.0.inner.backlog.lock().fast_open.set_limit(limit);
    }

    /// Closes the listener.
    ///
    /// Polling the iface is _always_ required after this method succeeds.
    ///
    /// Note that this method must be called before dropping the TCP listener to avoid resource
    /// leakage.
    pub fn close(&self) {
        // A TCP listener can be removed immediately.
        self.0.bound.iface().common().remove_tcp_listener(&self.0);

        let (connecting, connected) = {
            let mut socket = self.0.inner.backlog.lock();
            (
                core::mem::take(&mut socket.connecting),
                core::mem::take(&mut socket.connected),
            )
        };

        // The lock on `connecting`/`connected` cannot be locked after locking `self`, otherwise we
        // might get a deadlock due to inconsistent lock order problems.
        for socket in connecting.values().chain(connected.iter()) {
            socket.0.inner.lock().listener = None;
            socket.reset();
        }
    }
}

impl<E: Ext> RawTcpSetOption for TcpListener<E> {
    fn set_keep_alive(&self, interval: Option<Duration>) -> NeedIfacePoll {
        let mut backlog = self.0.inner.backlog.lock();
        backlog.socket.set_keep_alive(interval);

        NeedIfacePoll::FALSE
    }

    fn set_nagle_enabled(&self, enabled: bool) {
        let mut backlog = self.0.inner.backlog.lock();
        backlog.socket.set_nagle_enabled(enabled);
    }

    fn set_fast_open_retries(&self, limit: NonZeroU8) {
        self.0
            .inner
            .backlog
            .lock()
            .socket
            .set_fast_open_retries(limit);
    }
}

impl<E: Ext> TcpListenerBg<E> {
    pub(crate) const fn listener_key(&self) -> &ListenerKey {
        &self.inner.listener_key
    }
}

impl<E: Ext> TcpListenerBg<E> {
    /// Tries to process an incoming packet and returns whether the packet is processed.
    pub(crate) fn process(
        self: &Arc<Self>,
        iface: &mut PollableIfaceMut<E>,
        ip_repr: &IpRepr,
        tcp_repr: &TcpRepr,
        payload: PacketSlice<'_>,
    ) -> (TcpProcessResult, Option<Arc<TcpConnectionBg<E>>>) {
        let mut backlog = self.inner.backlog.lock();

        if !backlog
            .socket
            .accepts(iface.context_mut(), ip_repr, tcp_repr)
        {
            return (TcpProcessResult::NotProcessed, None);
        }

        backlog.last_receive_iface = Some(self.bound.iface().index());

        // FIXME: According to the Linux implementation, `max_conn` is the upper bound of
        // `connected.len()`. We currently limit it to `connected.len() + connecting.len()` for
        // simplicity.
        if backlog.connected.len() + backlog.connecting.len() >= backlog.max_conn {
            return (TcpProcessResult::Processed, None);
        }

        let pending_fast_open = {
            let TcpBacklog {
                socket, fast_open, ..
            } = &mut *backlog;
            fast_open.prepare::<E>(
                socket,
                iface.context_mut().now(),
                ip_repr.src_addr(),
                tcp_repr.fast_open,
                payload.len(),
            )
        };
        let result = match backlog
            .socket
            .process(iface.context_mut(), ip_repr, tcp_repr, payload)
        {
            None => TcpProcessResult::Processed,
            Some((ip_repr, tcp_repr)) => TcpProcessResult::ProcessedWithReply(ip_repr, tcp_repr),
        };

        if backlog.socket.state() == smoltcp::socket::tcp::State::Listen {
            return (result, None);
        }
        let early_accept = backlog.socket.fast_open_pending();
        let pending_fast_open = if early_accept {
            pending_fast_open
        } else {
            None
        };
        debug_assert_eq!(early_accept, pending_fast_open.is_some());

        let new_socket = {
            let mut socket = new_tcp_socket();
            RawTcpOption::inherit(&backlog.socket, &mut socket);
            socket.listen(backlog.socket.listen_endpoint()).unwrap();
            socket
        };

        let local_endpoint = backlog.socket.local_endpoint().unwrap();
        let conn = TcpConnection::new_cyclic(
            self.bound
                .iface()
                .bind_tcp(BindPortConfig::new_backlog(local_endpoint))
                .unwrap(),
            |weak| {
                TcpConnectionInner::new(
                    core::mem::replace(&mut backlog.socket, new_socket),
                    Some(self.clone()),
                    pending_fast_open,
                    weak,
                )
            },
        );
        let conn_bg = conn.inner().clone();

        if early_accept {
            backlog.connected.push(conn);
            self.notify_events(crate::socket::SocketEvents::CAN_RECV);
        } else {
            let old_conn = backlog.connecting.insert(*conn_bg.connection_key(), conn);
            debug_assert!(old_conn.is_none());
        }

        iface.update_next_poll_at_ms(&conn_bg, PollAt::Now);

        (result, Some(conn_bg))
    }
}
