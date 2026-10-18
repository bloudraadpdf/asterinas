// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::{
    errors::BindError,
    wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address},
};

use crate::{
    net::{
        iface::{Iface, iter_all_ifaces},
        route,
        socket::util::check_port_privilege,
    },
    prelude::*,
};

pub(super) fn resolve_bind_ifaces(endpoint: &IpEndpoint) -> Result<Vec<Arc<Iface>>> {
    check_port_privilege(endpoint.port)?;
    if !endpoint.addr.is_unspecified() {
        return Ok(vec![route::lookup_local_iface(endpoint.addr)?]);
    }
    let ifaces: Vec<_> = iter_all_ifaces()
        .filter(|iface| match endpoint.addr {
            IpAddress::Ipv4(_) => iface.ipv4_cidr().is_some(),
            IpAddress::Ipv6(_) => iface.ipv6_cidr().is_some(),
        })
        .cloned()
        .collect();
    if ifaces.is_empty() {
        return_errno_with_message!(Errno::EADDRNOTAVAIL, "no interface has this address family");
    }
    Ok(ifaces)
}

impl From<BindError> for Error {
    fn from(value: BindError) -> Self {
        match value {
            BindError::Exhausted => {
                Error::with_message(Errno::EAGAIN, "no ephemeral port is available")
            }
            BindError::InUse => {
                Error::with_message(Errno::EADDRINUSE, "the address is already in use")
            }
        }
    }
}

pub(super) fn get_ephemeral_endpoint(remote_endpoint: &IpEndpoint) -> Result<IpEndpoint> {
    let route_dst = remote_endpoint.addr;
    let iface = route::lookup_iface(route_dst)?;

    let source = match route_dst {
        IpAddress::Ipv4(_) => iface
            .ipv4_cidr()
            .map(|cidr| IpAddress::Ipv4(cidr.address()))
            .ok_or_else(|| {
                Error::with_message(
                    Errno::EADDRNOTAVAIL,
                    "the route output iface has no IPv4 address",
                )
            })?,
        IpAddress::Ipv6(_) => iface
            .ipv6_cidr()
            .map(|cidr| IpAddress::Ipv6(cidr.address()))
            .ok_or_else(|| {
                Error::with_message(
                    Errno::EADDRNOTAVAIL,
                    "the route output iface has no IPv6 address",
                )
            })?,
    };

    Ok(IpEndpoint::new(source, 0))
}

/// Maps an unspecified `connect` destination to the loopback address.
///
/// Linux treats `connect` to an unspecified address as connecting to localhost.
///
/// Reference: <https://elixir.bootlin.com/linux/v7.0/source/net/ipv4/route.c#L2803-L2812>.
/// Reference: <https://elixir.bootlin.com/linux/v7.0/source/net/ipv6/tcp_ipv6.c#L171-L181>.
pub(super) fn map_unspecified_to_localhost(mut endpoint: IpEndpoint) -> IpEndpoint {
    endpoint.addr = match endpoint.addr {
        IpAddress::Ipv4(address) if address.is_unspecified() => Ipv4Address::LOCALHOST.into(),
        IpAddress::Ipv6(address) if address.is_unspecified() => Ipv6Address::LOCALHOST.into(),
        address => address,
    };
    endpoint
}
