// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use aster_time::{read_monotonic_time, read_start_time};
use spin::Once;
use time::{Date, Month, PrimitiveDateTime, Time};

use crate::prelude::*;

/// This struct corresponds to `SystemTime` in Rust std.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SystemTime(PrimitiveDateTime);

static WALL_OFFSET: Once<SpinLock<Duration>> = Once::new();

pub(super) fn init() {
    let start_time = convert_system_time(read_start_time()).unwrap();
    WALL_OFFSET
        .call_once(|| SpinLock::new(start_time.duration_since(&SystemTime::UNIX_EPOCH).unwrap()));
}

impl SystemTime {
    /// The unix epoch, which represents 1970-01-01 00:00:00
    pub(crate) const UNIX_EPOCH: SystemTime = SystemTime::unix_epoch();

    const fn unix_epoch() -> Self {
        // 1970-01-01 00:00:00
        let Ok(date) = Date::from_ordinal_date(1970, 1) else {
            unreachable!()
        };
        let Ok(time) = Time::from_hms_nano(0, 0, 0, 0) else {
            unreachable!()
        };

        SystemTime(PrimitiveDateTime::new(date, time))
    }

    /// Returns the current system time
    pub(crate) fn now() -> Self {
        let offset = WALL_OFFSET.get().unwrap().disable_irq().lock();
        Self::UNIX_EPOCH
            .checked_add(*offset + read_monotonic_time())
            .unwrap()
    }

    pub(crate) fn boot_duration() -> Duration {
        *WALL_OFFSET.get().unwrap().disable_irq().lock()
    }

    pub(crate) fn realtime_at(monotonic: Duration) -> Duration {
        Self::boot_duration() + monotonic
    }

    pub(crate) fn set_realtime(realtime: Duration) -> Result<()> {
        if realtime.as_secs() >= (i64::MAX / super::NSEC_PER_SEC) as u64 {
            return_errno_with_message!(Errno::EINVAL, "system time exceeds the Linux clock range");
        }
        {
            let mut offset = WALL_OFFSET.get().unwrap().disable_irq().lock();
            *offset = realtime.checked_sub(read_monotonic_time()).ok_or_else(|| {
                Error::with_message(Errno::EINVAL, "real time precedes monotonic time")
            })?;
        }
        // Lock order: coarse clock or vDSO data -> wall offset, never the reverse.
        super::clocks::update_coarse_clock();
        #[cfg(not(target_arch = "loongarch64"))]
        crate::vdso::refresh_wall_clock();
        Ok(())
    }

    /// Add a duration to self. If the result does not exceed inner bounds return Some(t), else return None.
    pub(crate) fn checked_add(&self, duration: Duration) -> Option<Self> {
        let duration = convert_to_time_duration(duration);
        self.0.checked_add(duration).map(SystemTime)
    }

    /// Subtract a duration from self. If the result does not exceed inner bounds return Some(t), else return None.
    #[expect(dead_code)]
    pub(crate) fn checked_sub(&self, duration: Duration) -> Option<Self> {
        let duration = convert_to_time_duration(duration);
        self.0.checked_sub(duration).map(SystemTime)
    }

    /// Returns the duration since an earlier time. Return error if `earlier` is later than self.
    pub(crate) fn duration_since(&self, earlier: &SystemTime) -> Result<Duration> {
        if self.0 < earlier.0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "duration_since can only accept an earlier time"
            );
        }
        let duration = self.0 - earlier.0;
        Ok(convert_to_core_duration(duration))
    }

    /// Return the difference between current time and the time when self was created.
    /// Return Error if current time is earlier than creating time.
    /// The error can happen if self was created by checked_add.
    #[expect(dead_code)]
    pub(crate) fn elapsed(&self) -> Result<Duration> {
        let now = SystemTime::now();
        now.duration_since(self)
    }
}

/// convert ostd::time::Time to System time
fn convert_system_time(system_time: aster_time::SystemTime) -> Result<SystemTime> {
    let month = match Month::try_from(system_time.month) {
        Ok(month) => month,
        Err(_) => return_errno_with_message!(Errno::EINVAL, "unknown month in system time"),
    };
    let date = match Date::from_calendar_date(system_time.year as _, month, system_time.day) {
        Ok(date) => date,
        Err(_) => return_errno_with_message!(Errno::EINVAL, "Invalid system date"),
    };
    let time_ = match Time::from_hms_nano(
        system_time.hour,
        system_time.minute,
        system_time.second,
        system_time.nanos.try_into().unwrap(),
    ) {
        Ok(time_) => time_,
        Err(_) => return_errno_with_message!(Errno::EINVAL, "Invalid system time"),
    };
    Ok(SystemTime(PrimitiveDateTime::new(date, time_)))
}

/// FIXME: need to further check precision loss
/// convert core::time::Duration to time::Duration
const fn convert_to_time_duration(duration: Duration) -> time::Duration {
    let seconds = duration.as_secs() as i64;
    let nanoseconds = duration.subsec_nanos() as i32;
    time::Duration::new(seconds, nanoseconds)
}

/// FIXME: need to further check precision loss
/// convert time::Duration to core::time::Duration
const fn convert_to_core_duration(duration: time::Duration) -> Duration {
    let seconds = duration.whole_seconds() as u64;
    let nanoseconds = duration.subsec_nanoseconds() as u32;
    Duration::new(seconds, nanoseconds)
}
