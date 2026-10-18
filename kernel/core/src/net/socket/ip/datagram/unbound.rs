// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::{
    socket::UdpSocket,
    wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address},
};

use super::{bound::BoundDatagram, observer::DatagramObserver};
use crate::{
    events::IoEvents,
    net::{
        iface::BoundUdpPort,
        socket::{ip::common::resolve_bind_ifaces, util::datagram_common},
    },
    prelude::*,
    process::signal::Pollee,
};

pub(super) struct UnboundDatagram {
    _private: (),
}

impl UnboundDatagram {
    pub(super) fn new() -> Self {
        Self { _private: () }
    }
}

pub(super) struct BindOptions {
    pub(super) can_reuse: bool,
}

impl datagram_common::Unbound for UnboundDatagram {
    type Endpoint = IpEndpoint;
    type BindOptions = BindOptions;

    type Bound = BoundDatagram;

    fn bind(
        &mut self,
        endpoint: &Self::Endpoint,
        pollee: &Pollee,
        options: BindOptions,
    ) -> Result<Self::Bound> {
        let ports =
            BoundUdpPort::bind_many(resolve_bind_ifaces(endpoint)?, *endpoint, options.can_reuse)?;

        let bound_socket = match UdpSocket::new_bind(ports, DatagramObserver::new(pollee.clone())) {
            Ok(bound_socket) => bound_socket,
            Err((_, err)) => {
                unreachable!("`new_bind` fails with {:?}, which should not happen", err)
            }
        };

        Ok(BoundDatagram::new(bound_socket))
    }

    fn bind_ephemeral(
        &mut self,
        remote_endpoint: &Self::Endpoint,
        pollee: &Pollee,
    ) -> Result<Self::Bound> {
        let endpoint = {
            IpEndpoint::new(
                match remote_endpoint.addr {
                    IpAddress::Ipv4(_) => Ipv4Address::UNSPECIFIED.into(),
                    IpAddress::Ipv6(_) => Ipv6Address::UNSPECIFIED.into(),
                },
                0,
            )
        };
        self.bind(&endpoint, pollee, BindOptions { can_reuse: false })
    }

    fn check_io_events(&self) -> IoEvents {
        IoEvents::OUT
    }
}
