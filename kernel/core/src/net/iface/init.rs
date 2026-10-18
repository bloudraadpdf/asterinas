// SPDX-License-Identifier: MPL-2.0

use core::slice::Iter;

use aster_bigtcp::{
    device::{AnyNetworkDevice, WithDevice},
    iface::{InterfaceFlags, InterfaceName, InterfaceType},
};
#[cfg(feature = "virtio_net")]
use aster_softirq::BottomHalfDisabled;
use spin::Once;

use super::{Iface, poll::poll_ifaces};
use crate::{net::iface::sched::PollScheduler, prelude::*};

static IFACES: Once<Vec<Arc<Iface>>> = Once::new();

#[cfg(feature = "virtio_net")]
fn virtio_iface() -> Option<&'static Arc<Iface>> {
    IFACES.get().unwrap().get(1)
}

pub(in crate::net) fn iter_all_ifaces() -> Iter<'static, Arc<Iface>> {
    IFACES.get().unwrap().iter()
}

// TODO: Support multiple network devices and avoid the hardcoded device name.
#[cfg(feature = "virtio_net")]
const VIRTIO_DEVICE_NAME: &str = aster_virtio::device::network::DEVICE_NAME;

pub(in crate::net) fn init() {
    IFACES.call_once(|| {
        let ifaces = vec![new_loopback()];

        #[cfg(feature = "virtio_net")]
        let ifaces = {
            let mut ifaces = ifaces;

            // Initialize loopback before virtio
            // to ensure the loopback interface index is ahead of virtio.

            if let Some(iface_virtio) = new_virtio() {
                ifaces.push(iface_virtio);
            }

            ifaces
        };
        ifaces
    });

    #[cfg(feature = "virtio_net")]
    if let Some(iface_virtio) = virtio_iface() {
        let callback = || iface_virtio.poll();
        aster_network::register_recv_callback(VIRTIO_DEVICE_NAME, callback);
        aster_network::register_send_callback(VIRTIO_DEVICE_NAME, callback);
    }

    poll_ifaces();
}

fn new_loopback() -> Arc<Iface> {
    use aster_bigtcp::{
        device::Loopback,
        iface::IpIface,
        wire::{Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr},
    };

    const LOOPBACK_ADDRESS: Ipv4Address = Ipv4Address::new(127, 0, 0, 1);
    const LOOPBACK_ADDRESS_PREFIX_LEN: u8 = 8; // mask: 255.0.0.0
    const LOOPBACK_IPV6_ADDRESS: Ipv6Address = Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1);
    const LOOPBACK_IPV6_PREFIX_LEN: u8 = 128;

    struct Wrapper(Mutex<Loopback>);

    impl WithDevice for Wrapper {
        type Device = Loopback;

        fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&mut dyn AnyNetworkDevice) -> R,
        {
            let mut device = self.0.lock();
            f(&mut *device)
        }
    }

    // FIXME: These flags are currently hardcoded.
    // In the future, we should set appropriate values.
    let flags = InterfaceFlags::UP
        | InterfaceFlags::LOOPBACK
        | InterfaceFlags::RUNNING
        | InterfaceFlags::LOWER_UP;

    IpIface::new(
        Wrapper(Mutex::new(Loopback::new())),
        Ipv4Cidr::new(LOOPBACK_ADDRESS, LOOPBACK_ADDRESS_PREFIX_LEN),
        Some(Ipv6Cidr::new(
            LOOPBACK_IPV6_ADDRESS,
            LOOPBACK_IPV6_PREFIX_LEN,
        )),
        InterfaceName::from_str_truncated("lo"),
        PollScheduler::new(),
        InterfaceType::LOOPBACK,
        flags,
    ) as Arc<Iface>
}

#[cfg(feature = "virtio_net")]
fn new_virtio() -> Option<Arc<Iface>> {
    use aster_bigtcp::iface::EtherIface;

    let virtio_net = aster_network::get_device(VIRTIO_DEVICE_NAME)?;

    let ether_addr = virtio_net.lock().mac_addr();
    let config = super::ip_config::boot_config();

    struct Wrapper(Arc<SpinLock<dyn AnyNetworkDevice, BottomHalfDisabled>>);

    impl WithDevice for Wrapper {
        type Device = aster_virtio::device::network::device::NetworkDevice;

        fn with<F, R>(&self, f: F) -> R
        where
            F: FnOnce(&mut dyn AnyNetworkDevice) -> R,
        {
            let mut device = self.0.lock();
            f(&mut *device)
        }
    }

    // FIXME: These flags are currently hardcoded.
    // In the future, we should set appropriate values.
    let flags = InterfaceFlags::UP
        | InterfaceFlags::BROADCAST
        | InterfaceFlags::RUNNING
        | InterfaceFlags::MULTICAST
        | InterfaceFlags::LOWER_UP;

    Some(EtherIface::new(
        Wrapper(virtio_net),
        ether_addr,
        config.address,
        config.gateway,
        InterfaceName::from_str_truncated("eth0"),
        PollScheduler::new(),
        flags,
    ))
}
