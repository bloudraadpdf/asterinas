// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::{
    errors::tcp::ConnectError,
    socket::{ConnectState, RawTcpOption, RawTcpSetOption},
    wire::IpEndpoint,
};

use super::{
    binding::TcpBinding, connected::ConnectedStream, init::InitStream, observer::StreamObserver,
};
use crate::{
    events::IoEvents,
    net::iface::{BoundTcpPort, Iface, TcpConnection},
    prelude::*,
};

pub(super) struct ConnectingStream {
    tcp_conn: TcpConnection,
    remote_endpoint: IpEndpoint,
    other_ports: Vec<BoundTcpPort>,
}

pub(super) enum ConnResult {
    Connecting(ConnectingStream),
    Connected(ConnectedStream),
    Refused(InitStream),
}

impl ConnectingStream {
    pub(super) fn new(
        bound_port: BoundTcpPort,
        other_ports: Vec<BoundTcpPort>,
        remote_endpoint: IpEndpoint,
        option: &RawTcpOption,
        observer: StreamObserver,
    ) -> Result<Self, (Error, TcpBinding)> {
        let tcp_conn =
            match TcpConnection::new_connect(bound_port, remote_endpoint, option, observer) {
                Ok(tcp_conn) => tcp_conn,
                Err((bound_port, ConnectError::AddressInUse)) => {
                    return Err((
                        Error::with_message(Errno::EADDRNOTAVAIL, "connection key conflicts"),
                        TcpBinding::new(bound_port, other_ports),
                    ));
                }
                Err((bound_port, _)) => {
                    return Err((
                        Error::with_message(
                            Errno::ECONNREFUSED,
                            "the connection endpoint is not available",
                        ),
                        TcpBinding::new(bound_port, other_ports),
                    ));
                }
            };

        Ok(Self {
            tcp_conn,
            remote_endpoint,
            other_ports,
        })
    }

    pub(super) fn has_result(&self) -> bool {
        match self.tcp_conn.connect_state() {
            ConnectState::Connecting => false,
            ConnectState::Connected => true,
            ConnectState::Refused => true,
        }
    }

    pub(super) fn into_result(self) -> ConnResult {
        let next_state = self.tcp_conn.connect_state();

        match next_state {
            ConnectState::Connecting => ConnResult::Connecting(self),
            ConnectState::Connected => ConnResult::Connected(ConnectedStream::new(
                self.tcp_conn,
                self.remote_endpoint,
                true,
            )),
            ConnectState::Refused => {
                let bound_port = self.tcp_conn.into_bound_port().unwrap();
                ConnResult::Refused(InitStream::new_refused(TcpBinding::new(
                    bound_port,
                    self.other_ports,
                )))
            }
        }
    }

    pub(super) fn local_endpoint(&self) -> IpEndpoint {
        self.tcp_conn.local_endpoint()
    }

    pub(super) fn remote_endpoint(&self) -> IpEndpoint {
        self.remote_endpoint
    }

    pub(super) fn iface(&self) -> &Arc<Iface> {
        self.tcp_conn.iface()
    }

    pub(super) fn bound_port(&self) -> &BoundTcpPort {
        self.tcp_conn.bound_port()
    }

    pub(super) fn set_reuse_addr(&self, value: bool) {
        self.bound_port().set_can_reuse(value);
        self.other_ports
            .iter()
            .for_each(|port| port.set_can_reuse(value));
    }

    pub(super) fn check_io_events(&self) -> IoEvents {
        IoEvents::empty()
    }

    pub(super) fn set_raw_option<R>(
        &self,
        set_option: impl FnOnce(&dyn RawTcpSetOption) -> R,
    ) -> R {
        set_option(&self.tcp_conn)
    }

    pub(super) fn into_connection(self) -> TcpConnection {
        self.tcp_conn
    }
}
