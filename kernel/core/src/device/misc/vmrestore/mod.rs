// SPDX-License-Identifier: MPL-2.0

//! VMClock notifications and generation/entropy barriers for snapshot restore.

mod clock;
mod parameters;

use core::sync::atomic::{AtomicU32, Ordering};

use device_id::{DeviceId, MinorId};
use ostd::{
    arch::irq::{IRQ_CHIP, MappedIrqLine},
    irq::IrqLine,
    sync::{WaitQueue, rcu_barrier},
};
use spin::Once;

use self::clock::{CLOCK_BYTES, Clock};
use crate::{
    device::{Device, DeviceType, registry::char},
    events::IoEvents,
    fs::{
        devtmpfs::DevtmpfsNodeMeta,
        file::{PerOpenFileOps, StatusFlags},
        vfs::{inode::FileOps, path::Path},
    },
    prelude::*,
    process::signal::{PollHandle, Pollable, Pollee},
    util::{
        ioctl::RawIoctl,
        random::generation::{self, GenerationId},
    },
};

static STATE: Once<RestoreDriver> = Once::new();

struct RestoreDriver {
    state: Arc<RestoreState>,
    _interrupt: MappedIrqLine,
}

struct RestoreState {
    clock: Clock,
    pollee: Pollee,
    progress: SpinLock<GenerationProgress>,
    transport_wait: WaitQueue,
}

struct GenerationProgress {
    witnessed: GenerationId,
    transport: GenerationId,
}

impl RestoreState {
    fn refresh(&self) -> Result<()> {
        self.transport_wait.wait_until(|| {
            // Lock order: generation progress -> random stream. Never wait with either held.
            let mut progress = self.progress.disable_irq().lock();
            let id = match generation::current() {
                Ok(id) => id,
                Err(err) => return Some(Err(err)),
            };
            if progress.witnessed == id {
                return Some(Err(Error::with_message(
                    Errno::EALREADY,
                    "generation already refreshed",
                )));
            }
            if progress.transport != id {
                return None;
            }
            match generation::refresh() {
                Ok(refreshed) if refreshed == id => {
                    progress.witnessed = id;
                    Some(Ok(()))
                }
                Ok(_) => None,
                Err(err) => Some(Err(err)),
            }
        })
    }

    fn quiesce(&self) -> Result<()> {
        rcu_barrier();
        let progress = self.progress.disable_irq().lock();
        let id = generation::current()?;
        if progress.witnessed != id || progress.transport != id {
            return_errno_with_message!(Errno::EAGAIN, "restore generation is not ready");
        }
        generation::quiesce()?;
        if id != generation::current()? {
            return_errno!(Errno::EAGAIN);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Clock,
    Generation,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Clock => "vmclock0",
            Self::Generation => "vmgenid",
        }
    }
    fn minor(self) -> u32 {
        match self {
            Self::Clock => 241,
            Self::Generation => 242,
        }
    }
}

#[derive(Debug)]
struct RestoreDevice {
    id: DeviceId,
    kind: Kind,
}

impl Device for RestoreDevice {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }
    fn id(&self) -> DeviceId {
        self.id
    }
    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        Some(DevtmpfsNodeMeta::new(self.kind.name()).unwrap())
    }
    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        let state = STATE.get().unwrap().state.clone();
        Ok(Box::new(RestoreFile {
            sequence: AtomicU32::new(state.clock.sequence()?),
            state,
            kind: self.kind,
        }))
    }
}

struct RestoreFile {
    state: Arc<RestoreState>,
    kind: Kind,
    sequence: AtomicU32,
}

impl Pollable for RestoreFile {
    fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        if !matches!(self.kind, Kind::Clock) {
            return IoEvents::empty();
        }
        if let Some(poller) = poller {
            self.state.pollee.register_poller(poller, mask);
        }
        match self.state.clock.sequence() {
            Ok(sequence)
                if sequence & 1 == 0 && sequence != self.sequence.load(Ordering::Relaxed) =>
            {
                mask & IoEvents::IN
            }
            Ok(_) => IoEvents::empty(),
            Err(_) => IoEvents::ERR,
        }
    }
}

impl FileOps for RestoreFile {
    fn read_at(&self, offset: usize, writer: &mut VmWriter, _flags: StatusFlags) -> Result<usize> {
        if !matches!(self.kind, Kind::Clock) {
            return_errno!(Errno::EINVAL);
        }
        if offset >= CLOCK_BYTES || writer.avail() == 0 {
            return Ok(0);
        }
        let bytes = self.state.clock.snapshot()?;
        let end = offset + writer.avail().min(CLOCK_BYTES - offset);
        let copied = match writer.write_fallible(&mut VmReader::from(&bytes[offset..end])) {
            Ok(copied) => copied,
            Err((_, copied)) if copied > 0 => copied,
            Err((error, _)) => return Err(error.into()),
        };
        self.sequence.store(
            u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            Ordering::Relaxed,
        );
        Ok(copied)
    }
    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _flags: StatusFlags,
    ) -> Result<usize> {
        return_errno!(Errno::EBADF);
    }
}

impl PerOpenFileOps for RestoreFile {
    fn check_seekable(&self) -> Result<()> {
        match self.kind {
            Kind::Clock => Ok(()),
            Kind::Generation => return_errno!(Errno::ESPIPE),
        }
    }
    fn is_offset_aware(&self) -> bool {
        matches!(self.kind, Kind::Clock)
    }
    fn ioctl(&self, _path: &Path, ioctl: RawIoctl) -> Result<i32> {
        if !matches!(self.kind, Kind::Generation) {
            return_errno!(Errno::ENOTTY);
        }
        match ioctl.cmd() {
            0x5600 => self.state.refresh()?,
            0x5601 => self.state.quiesce()?,
            _ => return_errno!(Errno::ENOTTY),
        }
        Ok(0)
    }
}

pub(crate) fn transport_restored() {
    let Some(driver) = STATE.get() else {
        return;
    };
    let id = generation::current().expect("cannot read restored generation");
    driver.state.progress.disable_irq().lock().transport = id;
    driver.state.transport_wait.wake_all();
}

pub(super) fn init() -> Result<()> {
    let (clock, genid, gsi) = parameters::acquire()?;
    let id = generation::install(genid)?;
    let state = Arc::new(RestoreState {
        clock,
        pollee: Pollee::new(),
        progress: SpinLock::new(GenerationProgress {
            witnessed: id,
            transport: id,
        }),
        transport_wait: WaitQueue::new(),
    });
    let mut irq = IrqLine::alloc()?;
    let weak = Arc::downgrade(&state);
    irq.on_active(move |_| {
        generation::refresh().expect("snapshot entropy refresh failed");
        if let Some(state) = weak.upgrade() {
            state.pollee.notify(IoEvents::IN);
        }
    });
    let interrupt = IRQ_CHIP.get().unwrap().map_gsi_pin_to(irq, gsi)?;
    STATE.call_once(|| RestoreDriver {
        state,
        _interrupt: interrupt,
    });
    let major = super::MISC_MAJOR.get().unwrap().get();
    for kind in [Kind::Clock, Kind::Generation] {
        char::register(Arc::new(RestoreDevice {
            id: DeviceId::new(major, MinorId::new(kind.minor())),
            kind,
        }))?;
    }
    Ok(())
}
