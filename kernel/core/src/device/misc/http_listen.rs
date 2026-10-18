// SPDX-License-Identifier: MPL-2.0

//! Privileged notification of a successful IPv4 TCP listen operation.

use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use aster_bigtcp::wire::{IpAddress, IpEndpoint};
use device_id::{DeviceId, MinorId};
use spin::Once;

use crate::{
    device::{Device, DeviceType, registry::char},
    events::IoEvents,
    fs::{
        devtmpfs::DevtmpfsNodeMeta,
        file::{PerOpenFileOps, StatusFlags},
        vfs::inode::FileOps,
    },
    prelude::*,
    process::signal::{PollHandle, Pollable, Pollee},
};

const HTTP_LISTEN_MINOR: u32 = 240;
static STATE: Once<Arc<ListenState>> = Once::new();

struct ListenState {
    port: AtomicU16,
    ready: AtomicBool,
    pollee: Pollee,
}

#[derive(Debug)]
struct ListenDevice {
    id: DeviceId,
}

impl Device for ListenDevice {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        Some(DevtmpfsNodeMeta::new("http-listen").unwrap())
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        Ok(Box::new(ListenFile(STATE.get().unwrap().clone())))
    }
}

struct ListenFile(Arc<ListenState>);

impl Pollable for ListenFile {
    fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        self.0.pollee.poll_with(mask, poller, || {
            if self.0.ready.load(Ordering::Acquire) {
                IoEvents::IN
            } else {
                IoEvents::empty()
            }
        })
    }
}

impl FileOps for ListenFile {
    fn read_at(
        &self,
        _offset: usize,
        _writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        return_errno_with_message!(Errno::EBADF, "the listen watch does not provide read data");
    }

    fn write_at(
        &self,
        _offset: usize,
        reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        if reader.remain() != size_of::<u16>() {
            return_errno_with_message!(Errno::EINVAL, "the listen watch requires one port");
        }
        let port = u16::from_be(reader.read_val()?);
        if port == 0 {
            return_errno_with_message!(Errno::EINVAL, "the listen watch port is zero");
        }
        self.0
            .port
            .compare_exchange(0, port, Ordering::Release, Ordering::Relaxed)
            .map_err(|_| Error::with_message(Errno::EBUSY, "a listen watch is already armed"))?;
        Ok(size_of::<u16>())
    }
}

impl PerOpenFileOps for ListenFile {
    fn check_seekable(&self) -> Result<()> {
        return_errno_with_message!(Errno::ESPIPE, "the listen watch is not seekable");
    }

    fn is_offset_aware(&self) -> bool {
        false
    }
}

pub(crate) fn on_listen(endpoint: IpEndpoint) {
    if !matches!(endpoint.addr, IpAddress::Ipv4(_)) {
        return;
    }
    let Some(state) = STATE.get() else {
        return;
    };
    if state.port.load(Ordering::Acquire) == endpoint.port {
        state.ready.store(true, Ordering::Release);
        state.pollee.notify(IoEvents::IN);
    }
}

pub(super) fn init() {
    STATE.call_once(|| {
        Arc::new(ListenState {
            port: AtomicU16::new(0),
            ready: AtomicBool::new(false),
            pollee: Pollee::new(),
        })
    });
    let major = super::MISC_MAJOR.get().unwrap().get();
    char::register(Arc::new(ListenDevice {
        id: DeviceId::new(major, MinorId::new(HTTP_LISTEN_MINOR)),
    }))
    .unwrap();
}
