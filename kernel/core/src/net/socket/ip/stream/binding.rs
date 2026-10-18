// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::{
    iface::BindPortConfig,
    wire::{IpAddress, IpEndpoint},
};

use crate::{
    net::{
        iface::{BoundTcpPort, iter_all_ifaces},
        route,
        socket::util::check_port_privilege,
    },
    prelude::*,
};

pub(super) struct TcpBinding {
    first: BoundTcpPort,
    others: Vec<BoundTcpPort>,
}

impl TcpBinding {
    pub(super) fn new(first: BoundTcpPort, others: Vec<BoundTcpPort>) -> Self {
        Self { first, others }
    }

    pub(super) fn bind(endpoint: &IpEndpoint, can_reuse: bool) -> Result<Self> {
        check_port_privilege(endpoint.port)?;
        let unspecified = match endpoint.addr {
            IpAddress::Ipv4(address) => address.is_unspecified(),
            IpAddress::Ipv6(address) => address.is_unspecified(),
        };
        if !unspecified {
            let iface = route::lookup_local_iface(endpoint.addr)?;
            let first = iface.bind_tcp(BindPortConfig::new(*endpoint, can_reuse))?;
            return Ok(Self::new(first, Vec::new()));
        }
        let ifaces: Vec<_> = iter_all_ifaces()
            .filter(|iface| match endpoint.addr {
                IpAddress::Ipv4(_) => iface.ipv4_cidr().is_some(),
                IpAddress::Ipv6(_) => iface.ipv6_cidr().is_some(),
            })
            .cloned()
            .collect();
        if ifaces.is_empty() {
            return_errno_with_message!(
                Errno::EADDRNOTAVAIL,
                "no interface has this address family"
            );
        }
        let mut ports = BoundTcpPort::bind_many(ifaces, *endpoint, can_reuse)?;
        let first = ports.remove(0);
        Ok(Self::new(first, ports))
    }

    pub(super) fn endpoint(&self) -> IpEndpoint {
        self.first.endpoint()
    }

    pub(super) fn set_can_reuse(&self, value: bool) {
        self.first.set_can_reuse(value);
        self.others
            .iter()
            .for_each(|port| port.set_can_reuse(value));
    }

    pub(super) fn select(
        self,
        destination: IpAddress,
    ) -> Result<(BoundTcpPort, Vec<BoundTcpPort>), (Error, Self)> {
        if self.others.is_empty() {
            return Ok((self.first, self.others));
        }
        let iface = match route::lookup_iface(destination) {
            Ok(iface) => iface,
            Err(error) => return Err((error, self)),
        };
        if Arc::ptr_eq(self.first.iface(), &iface) {
            return Ok((self.first, self.others));
        }
        let Some(index) = self
            .others
            .iter()
            .position(|port| Arc::ptr_eq(port.iface(), &iface))
        else {
            return Err((
                Error::with_message(
                    Errno::EADDRNOTAVAIL,
                    "the output interface has no bound port",
                ),
                self,
            ));
        };
        let mut others = self.others;
        let selected = others.swap_remove(index);
        others.push(self.first);
        Ok((selected, others))
    }

    pub(super) fn into_ports(self) -> Vec<BoundTcpPort> {
        let mut ports = self.others;
        ports.push(self.first);
        ports
    }

    pub(super) fn from_ports(mut ports: Vec<BoundTcpPort>) -> Self {
        let first = ports.pop().unwrap();
        Self::new(first, ports)
    }
}
