// SPDX-License-Identifier: MPL-2.0

//! TTY devices.
//!
//! This module implements TTY devices such as `/dev/tty0`, `/dev/tty`, and `/dev/console`.
//!
//! Reference: <https://www.kernel.org/doc/html/latest/admin-guide/devices.html>

use device_id::{DeviceId, MajorId, MinorId};
use spin::Once;

use crate::{
    device::{Device, DeviceType, registry::char, tty::serial::serial0_device},
    fs::{
        devtmpfs::DevtmpfsNodeMeta,
        file::{PerOpenFileOps, mkmod},
    },
    prelude::*,
};

/// Corresponds to `/dev/tty0` in the file system. This device represents the active virtual
/// terminal.
#[cfg(feature = "console_video")]
pub(crate) struct Tty0Device;

#[cfg(feature = "console_video")]
impl Device for Tty0Device {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        DeviceId::new(MajorId::new(4), MinorId::new(0))
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        Some(DevtmpfsNodeMeta::new("tty0").unwrap())
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        super::vt::active_vt().open()
    }
}

/// Corresponds to `/dev/tty` in the file system. This device represents the controlling terminal
/// of the session of the current process.
pub(crate) struct TtyDevice;

impl Device for TtyDevice {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        DeviceId::new(MajorId::new(5), MinorId::new(0))
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        // Reference: <https://elixir.bootlin.com/linux/v6.18/source/drivers/tty/tty_io.c#L3511>.
        Some(DevtmpfsNodeMeta::with_mode("tty", mkmod!(a+rw)).unwrap())
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        let Some(terminal) = current!().terminal() else {
            return_errno_with_message!(
                Errno::ENOTTY,
                "the process does not have a controlling terminal"
            );
        };

        terminal.open()
    }
}

/// Corresponds to `/dev/console` in the file system. This device represents a console to which
/// system messages will be sent.
pub(crate) struct SystemConsole {
    inner: Arc<dyn Device>,
}

/// The device ID of `/dev/console`.
pub(crate) const CONSOLE_DEVICE_ID: DeviceId = DeviceId::new(MajorId::new(5), MinorId::new(1));

impl SystemConsole {
    /// Returns the singleton instance of the console device.
    pub(crate) fn singleton() -> Result<&'static Arc<SystemConsole>> {
        static INSTANCE: Once<Arc<SystemConsole>> = Once::new();

        INSTANCE.try_call_once(|| {
            // TODO: Support specifying multiple TTY devices, e.g., "console=hvc0 console=tty0".
            let console_name = CONSOLES
                .get()
                .and_then(|consoles| consoles.first().map(|s| s.as_str()))
                .unwrap_or(if cfg!(feature = "console_video") {
                    "tty0"
                } else {
                    "ttyS0"
                });

            let device = match console_name {
                #[cfg(feature = "console_video")]
                "tty0" => Some(Arc::new(Tty0Device) as _),
                "ttyS0" => serial0_device().cloned().map(|device| device as _),
                #[cfg(feature = "virtio_devices")]
                "hvc0" => super::hvc::hvc0_device().cloned().map(|device| device as _),
                _ => None,
            };
            let inner = if let Some(device) = device {
                device
            } else {
                warn!(
                    "'{}' console not found, using the default console",
                    console_name
                );
                #[cfg(feature = "console_video")]
                {
                    Arc::new(Tty0Device) as Arc<dyn Device>
                }
                #[cfg(not(feature = "console_video"))]
                {
                    serial0_device()
                        .ok_or_else(|| {
                            Error::with_message(Errno::ENODEV, "serial console is unavailable")
                        })?
                        .clone() as Arc<dyn Device>
                }
            };

            Ok(Arc::new(Self { inner }))
        })
    }
}

impl Device for SystemConsole {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        CONSOLE_DEVICE_ID
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        Some(DevtmpfsNodeMeta::new("console").unwrap())
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        self.inner.open()
    }
}

pub(super) fn init_in_first_process() -> Result<()> {
    #[cfg(feature = "console_video")]
    char::register(Arc::new(Tty0Device))?;
    char::register(Arc::new(TtyDevice))?;
    char::register(SystemConsole::singleton()?.clone())?;

    Ok(())
}

static CONSOLES: Once<Vec<String>> = Once::new();
aster_cmdline::define_repeatable_kv_param!("console", CONSOLES);
