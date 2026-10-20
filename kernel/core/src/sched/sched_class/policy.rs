// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{AtomicU8, Ordering::Relaxed};

use atomic_integer_wrapper::define_atomic_version_of_integer_like_type;
use int_to_c_enum::TryFromInt;
use ostd::sync::SpinLock;

pub(crate) use super::real_time::{RealTimePolicy, RealTimePriority};
use crate::sched::nice::Nice;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SchedPriority(u8);

impl SchedPriority {
    pub(crate) fn from_policy(policy: SchedPolicy) -> Self {
        Self(match policy {
            SchedPolicy::Stop => 0,
            SchedPolicy::RealTime { rt_prio, .. } => rt_prio.get(),
            SchedPolicy::Fair(nice) => (120 + i16::from(nice.value().get())) as u8,
            SchedPolicy::Idle => 140,
        })
    }

    fn real_time(self) -> Option<RealTimePriority> {
        RealTimePriority::try_from(self.0).ok()
    }
}

/// The User-chosen scheduling policy.
///
/// The scheduling policies are specified by the user, usually through its priority.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum SchedPolicy {
    #[expect(dead_code)]
    Stop,
    RealTime {
        rt_prio: RealTimePriority,
        rt_policy: RealTimePolicy,
    },
    Fair(Nice),
    Idle,
}

impl Default for SchedPolicy {
    fn default() -> Self {
        Self::Fair(Nice::default())
    }
}

/// The Linux scheduling policy code.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.17.7/source/include/uapi/linux/sched.h#L112>.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, TryFromInt)]
pub(crate) enum LinuxSchedPolicy {
    Normal = 0,
    Fifo = 1,
    RoundRobin = 2,
    Batch = 3, // Not supported.
    Iso = 4,   // Reserved but not implemented yet on Linux.
    Idle = 5,
    Deadline = 6, // Not supported.
    Ext = 7,      // Not supported.
}

/// Projects internal scheduling policies onto Linux's user-visible policy codes.
///
/// This projection is many-to-one: `Stop` is reported as `Fifo`, and the
/// kernel-only `Idle` policy is reported as `Normal`.
impl From<SchedPolicy> for LinuxSchedPolicy {
    fn from(policy: SchedPolicy) -> Self {
        match policy {
            SchedPolicy::Stop => LinuxSchedPolicy::Fifo,
            SchedPolicy::RealTime {
                rt_policy: RealTimePolicy::Fifo,
                ..
            } => LinuxSchedPolicy::Fifo,
            SchedPolicy::RealTime {
                rt_policy: RealTimePolicy::RoundRobin { .. },
                ..
            } => LinuxSchedPolicy::RoundRobin,
            SchedPolicy::Fair(_) | SchedPolicy::Idle => LinuxSchedPolicy::Normal,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, TryFromInt)]
pub(super) enum SchedPolicyKind {
    Stop = 0,
    RealTime = 1,
    Fair = 2,
    Idle = 3,
}

impl SchedPolicy {
    pub(super) fn kind(&self) -> SchedPolicyKind {
        match self {
            SchedPolicy::Stop => SchedPolicyKind::Stop,
            SchedPolicy::RealTime { .. } => SchedPolicyKind::RealTime,
            SchedPolicy::Fair(_) => SchedPolicyKind::Fair,
            SchedPolicy::Idle => SchedPolicyKind::Idle,
        }
    }
}

define_atomic_version_of_integer_like_type!(SchedPolicyKind, try_from = true, {
    #[derive(Debug)]
    pub(crate) struct AtomicSchedPolicyKind(AtomicU8);
});

impl From<SchedPolicyKind> for u8 {
    fn from(value: SchedPolicyKind) -> Self {
        value as _
    }
}

#[derive(Debug)]
pub(super) struct SchedPolicyState {
    kind: AtomicSchedPolicyKind,
    policy: SpinLock<PolicyState>,
}

#[derive(Debug)]
struct PolicyState {
    configured: SchedPolicy,
    inherited: Option<SchedPriority>,
}

impl PolicyState {
    fn priority(&self) -> SchedPriority {
        let configured = SchedPriority::from_policy(self.configured);
        self.inherited
            .map_or(configured, |inherited| inherited.min(configured))
    }

    fn effective(&self) -> SchedPolicy {
        let priority = self.priority();
        if priority >= SchedPriority::from_policy(self.configured) {
            return self.configured;
        }
        let Some(rt_prio) = priority.real_time() else {
            return self.configured;
        };
        let rt_policy = match self.configured {
            SchedPolicy::RealTime { rt_policy, .. } => rt_policy,
            _ => RealTimePolicy::Fifo,
        };
        SchedPolicy::RealTime { rt_prio, rt_policy }
    }
}

impl SchedPolicyState {
    pub(crate) fn new(policy: SchedPolicy) -> Self {
        Self {
            kind: AtomicSchedPolicyKind::new(policy.kind()),
            policy: SpinLock::new(PolicyState {
                configured: policy,
                inherited: None,
            }),
        }
    }

    pub(crate) fn kind(&self) -> SchedPolicyKind {
        self.kind.load(Relaxed)
    }

    pub(crate) fn get(&self) -> SchedPolicy {
        self.policy.disable_irq().lock().configured
    }

    pub(crate) fn effective(&self) -> SchedPolicy {
        self.policy.disable_irq().lock().effective()
    }

    pub(crate) fn priority(&self) -> SchedPriority {
        self.policy.disable_irq().lock().priority()
    }

    pub(crate) fn set(&self, mut policy: SchedPolicy, update: impl FnOnce(SchedPolicy)) {
        let mut this = self.policy.disable_irq().lock();

        // Keep the old base slice factor if the new policy doesn't specify one.
        if let (
            SchedPolicy::RealTime {
                rt_policy:
                    RealTimePolicy::RoundRobin {
                        base_slice_factor: slot,
                    },
                ..
            },
            SchedPolicy::RealTime {
                rt_policy: RealTimePolicy::RoundRobin { base_slice_factor },
                ..
            },
        ) = (this.configured, &mut policy)
        {
            *base_slice_factor = slot.or(*base_slice_factor);
        }

        this.configured = policy;
        let effective = this.effective();
        update(effective);
        self.kind.store(effective.kind(), Relaxed);
    }

    pub(crate) fn set_inherited(
        &self,
        priority: Option<SchedPriority>,
        apply: impl FnOnce(SchedPolicy),
    ) {
        let mut this = self.policy.disable_irq().lock();
        let previous = this.effective();
        this.inherited = priority;
        let effective = this.effective();
        if previous != effective {
            apply(effective);
            self.kind.store(effective.kind(), Relaxed);
        }
    }
}
