// SPDX-License-Identifier: MPL-2.0

use smoltcp::wire::{IpAddress, IpEndpoint};

/// The configuration using for bind to a TCP/UDP port.
pub struct BindPortConfig {
    addr: IpAddress,
    kind: PortKind,
}

enum PortKind {
    /// Binds to the specified reusable port.
    CanReuse(u16),
    /// Binds to the specified non-reusable port.
    Specified(u16),
    /// Allocates an ephemeral port to bind.
    Ephemeral(bool),
    /// Reuses the port of the listening socket.
    Backlog(u16),
}

impl BindPortConfig {
    /// Creates new configuration using for bind to a TCP/UDP port.
    pub fn new(endpoint: IpEndpoint, can_reuse: bool) -> Self {
        let port = endpoint.port;
        let kind = match (port, can_reuse) {
            (0, can_reuse) => PortKind::Ephemeral(can_reuse),
            (_, true) => PortKind::CanReuse(port),
            (_, false) => PortKind::Specified(port),
        };
        Self {
            addr: endpoint.addr,
            kind,
        }
    }

    /// Creates a new configuration for reusing the port of a listening socket.
    pub fn new_backlog(endpoint: IpEndpoint) -> Self {
        Self {
            addr: endpoint.addr,
            kind: PortKind::Backlog(endpoint.port),
        }
    }

    pub(super) fn is_backlog(&self) -> bool {
        matches!(self.kind, PortKind::Backlog(..))
    }

    pub(super) fn can_reuse(&self) -> bool {
        matches!(
            self.kind,
            PortKind::CanReuse(..) | PortKind::Ephemeral(true)
        )
    }

    pub(super) fn port(&self) -> Option<u16> {
        match &self.kind {
            PortKind::CanReuse(port) | PortKind::Specified(port) | PortKind::Backlog(port) => {
                Some(*port)
            }
            PortKind::Ephemeral(_) => None,
        }
    }

    pub(super) fn addr(&self) -> IpAddress {
        self.addr
    }
}

pub(crate) fn addresses_overlap(left: IpAddress, right: IpAddress) -> bool {
    let normalize = |address| match address {
        IpAddress::Ipv6(address) => address
            .to_ipv4_mapped()
            .map(IpAddress::Ipv4)
            .unwrap_or(IpAddress::Ipv6(address)),
        address => address,
    };
    match (normalize(left), normalize(right)) {
        (IpAddress::Ipv4(left), IpAddress::Ipv4(right)) => {
            left == right || left.is_unspecified() || right.is_unspecified()
        }
        (IpAddress::Ipv6(left), IpAddress::Ipv6(right)) => {
            left == right || left.is_unspecified() || right.is_unspecified()
        }
        _ => false,
    }
}
