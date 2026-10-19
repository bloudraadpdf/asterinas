// SPDX-License-Identifier: MPL-2.0

#[cfg(feature = "console_video")]
use ostd::info;

pub(crate) fn init() {
    #[cfg(feature = "console_video")]
    for device in aster_input::all_devices() {
        info!("Found an input device, name: {}", device.name());
    }
}
