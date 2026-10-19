// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use super::ControlMessage;
use crate::{
    net::socket::options::{
        SocketOption, Timestamp, TimestampNew, TimestampNs, TimestampNsNew,
        macros::{sock_option_mut, sock_option_ref},
    },
    prelude::*,
};

#[derive(Clone, Copy, Debug)]
#[repr(i32)]
pub(crate) enum TimestampMode {
    Microseconds = 29,
    Nanoseconds = 35,
    MicrosecondsNew = 63,
    NanosecondsNew = 64,
}

impl TimestampMode {
    pub(crate) fn is_nanoseconds(self) -> bool {
        matches!(self, Self::Nanoseconds | Self::NanosecondsNew)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TimestampOptions {
    mode: Option<TimestampMode>,
}

impl TimestampOptions {
    pub(crate) fn get_option(&self, option: &mut dyn SocketOption) -> bool {
        let nanoseconds = self.mode.is_some_and(TimestampMode::is_nanoseconds);
        let microseconds = self.mode.is_some() && !nanoseconds;
        sock_option_mut!(match option {
            value @ Timestamp => {
                value.set(microseconds);
            }
            value @ TimestampNew => {
                value.set(microseconds);
            }
            value @ TimestampNs => {
                value.set(nanoseconds);
            }
            value @ TimestampNsNew => {
                value.set(nanoseconds);
            }
            _ => return false,
        });
        true
    }

    pub(crate) fn set_option(&mut self, option: &dyn SocketOption) -> bool {
        let (enabled, mode) = sock_option_ref!(match option {
            value @ Timestamp => {
                (*value.get().unwrap(), TimestampMode::Microseconds)
            }
            value @ TimestampNew => {
                (*value.get().unwrap(), TimestampMode::MicrosecondsNew)
            }
            value @ TimestampNs => {
                (*value.get().unwrap(), TimestampMode::Nanoseconds)
            }
            value @ TimestampNsNew => {
                (*value.get().unwrap(), TimestampMode::NanosecondsNew)
            }
            _ => return false,
        });
        self.mode = enabled.then_some(mode);
        true
    }

    pub(crate) fn control_message(&self, stamp: Duration) -> Option<ControlMessage> {
        self.mode
            .map(|mode| ControlMessage::Timestamp { mode, stamp })
    }
}
