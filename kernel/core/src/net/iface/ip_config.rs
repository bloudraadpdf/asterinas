// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::wire::{Ipv4Address, Ipv4Cidr};
use spin::Once;

#[derive(Clone, Copy, Debug)]
pub(super) struct IpConfig {
    pub(super) address: Ipv4Cidr,
    pub(super) gateway: Ipv4Address,
}

static BOOT_CONFIG: Once<Result<IpConfig, ()>> = Once::new();

aster_cmdline::submit! {
    aster_cmdline::KernelParam::new("ip", setup, false)
}

fn setup(occurrences: &[Option<&str>]) {
    BOOT_CONFIG.call_once(|| {
        let [Some(value)] = occurrences else {
            return Err(());
        };
        IpConfig::parse(value)
    });
}

pub(super) fn boot_config() -> IpConfig {
    match BOOT_CONFIG.get() {
        Some(Ok(config)) => *config,
        Some(Err(())) => panic!("invalid static IPv4 kernel parameter"),
        None => IpConfig {
            address: Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24),
            gateway: Ipv4Address::new(10, 0, 2, 2),
        },
    }
}

impl IpConfig {
    fn parse(value: &str) -> Result<Self, ()> {
        // Linux static IP syntax: client:server:gateway:netmask:hostname:device:autoconf.
        let fields: [&str; 7] = value
            .split(':')
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .map_err(|_| ())?;
        let [client, server, gateway, mask, hostname, device, autoconf] = fields;
        if !server.is_empty()
            || !hostname.is_empty()
            || device != "eth0"
            || !matches!(autoconf, "off" | "none")
        {
            return Err(());
        }
        let address: Ipv4Address = client.parse().map_err(|_| ())?;
        let gateway: Ipv4Address = gateway.parse().map_err(|_| ())?;
        let mask: Ipv4Address = mask.parse().map_err(|_| ())?;
        let mask = u32::from_be_bytes(mask.octets());
        let prefix = mask.leading_ones() as u8;
        if prefix == 0 || prefix > 30 || mask != u32::MAX << (32 - prefix) {
            return Err(());
        }
        let address = Ipv4Cidr::new(address, prefix);
        let invalid_host = |ip: Ipv4Address| {
            ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || ip == address.network().address()
                || Some(ip) == address.broadcast()
        };
        if invalid_host(address.address())
            || invalid_host(gateway)
            || !address.contains_addr(&gateway)
            || gateway == address.address()
        {
            return Err(());
        }
        Ok(Self { address, gateway })
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn static_ipv4_subnet() {
        let config = IpConfig::parse("172.30.0.2::172.30.0.1:255.255.255.252::eth0:off").unwrap();
        assert_eq!(
            config.address,
            Ipv4Cidr::new(Ipv4Address::new(172, 30, 0, 2), 30)
        );
        assert_eq!(config.gateway, Ipv4Address::new(172, 30, 0, 1));
    }

    #[ktest]
    fn invalid_static_ipv4_is_rejected() {
        for value in [
            "dhcp",
            "",
            "172.30.0.2::172.30.0.1:255.0.255.0::eth0:off",
            "172.30.0.0::172.30.0.1:255.255.255.252::eth0:off",
            "172.30.0.3::172.30.0.1:255.255.255.252::eth0:off",
            "172.30.0.2::172.30.0.5:255.255.255.252::eth0:off",
            "127.0.0.2::127.0.0.1:255.255.255.0::eth0:off",
            "172.30.0.2::172.30.0.1:255.255.255.252::eth1:off",
            "172.30.0.2::172.30.0.1:255.255.255.252::eth0:dhcp",
            "172.30.0.2::172.30.0.1:255.255.255.252::eth0:off:1.1.1.1",
        ] {
            assert!(IpConfig::parse(value).is_err(), "accepted {value}");
        }
    }
}
