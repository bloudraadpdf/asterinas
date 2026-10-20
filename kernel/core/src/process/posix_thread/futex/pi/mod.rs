// SPDX-License-Identifier: MPL-2.0

mod engine;
mod queue;
mod state;

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use atomic_integer_wrapper::define_atomic_version_of_integer_like_type;
pub(crate) use engine::{PiLockMode, lock_pi, unlock_pi};
use ostd::sync::{LocalIrqDisabled, WaitQueue};
use spin::Once;

use self::state::{Effects, PiGraph, PiWaiter, ThreadRef};
use super::{FutexKey, FutexWaitKind, get_futex_bucket};
use crate::{prelude::*, process::PidNamespace, sched::SchedPolicy, thread::Thread};

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromInt)]
enum ExitPhase {
    Active = 0,
    Exiting = 1,
    Exited = 2,
}

impl From<ExitPhase> for u8 {
    fn from(value: ExitPhase) -> Self {
        value as u8
    }
}

define_atomic_version_of_integer_like_type!(ExitPhase, try_from = true, {
    struct AtomicExitPhase(AtomicU8);
});

pub(crate) struct PiExitState {
    phase: AtomicExitPhase,
    generation: AtomicU64,
    changed: WaitQueue,
}

impl PiExitState {
    pub(crate) fn new() -> Self {
        Self {
            phase: AtomicExitPhase::new(ExitPhase::Active),
            generation: AtomicU64::new(0),
            changed: WaitQueue::new(),
        }
    }

    fn phase(&self) -> ExitPhase {
        self.phase.load(Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn begin(&self) {
        self.phase.store(ExitPhase::Exiting, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn complete(&self) {
        self.phase.store(ExitPhase::Exited, Ordering::Release);
    }

    fn wait(&self) -> Result<()> {
        self.changed
            .pause_until(|| (self.phase() != ExitPhase::Exiting).then_some(()))
    }
}

static GRAPH: Once<SpinLock<PiGraph, LocalIrqDisabled>> = Once::new();

fn graph() -> &'static SpinLock<PiGraph, LocalIrqDisabled> {
    GRAPH.call_once(|| SpinLock::new(PiGraph::new()))
}

pub(crate) fn update_sched_policy(
    thread: Arc<Thread>,
    namespace: &PidNamespace,
    update: impl FnOnce(SchedPolicy) -> Result<SchedPolicy>,
) -> Result<()> {
    let reference = ThreadRef::new(thread, namespace)?;
    let mut effects = Effects::new();
    let result = graph()
        .lock()
        .update_policy(&reference, update, &mut effects);
    effects.flush();
    result
}

fn remove_marker(key: &FutexKey, waiter: &PiWaiter) {
    if let Some(marker) = get_futex_bucket(key)
        .1
        .lock()
        .remove_by_waker(&waiter.waker)
    {
        debug_assert!(marker.kind == FutexWaitKind::PriorityInheritance);
    }
}

pub(crate) fn begin_exit(ctx: &Context) {
    let _graph = graph().lock();
    ctx.posix_thread.pi_exit().begin();
}

pub(crate) fn release_owned(ctx: &Context) {
    let reference = ThreadRef::new(Thread::current().unwrap(), ctx.process.pid_namespace())
        .expect("a POSIX thread is visible in its own PID namespace");
    let mut effects = Effects::new();
    {
        let mut graph = graph().lock();
        if let Some((key, waiter)) = graph.cancel_thread_wait(reference.id(), &mut effects) {
            remove_marker(&key, &waiter);
        }
    }
    effects.flush();
    loop {
        let mut effects = Effects::new();
        let (complete, wake) = {
            let mut graph = graph().lock();
            if let Some(id) = graph.first_owned(reference.id()) {
                let key = graph.states.get(&id).unwrap().key.clone();
                let wake = if graph.has_waiters(id) {
                    graph.grant_next(id, true, true, &mut effects)
                } else {
                    let wake = graph.abandon_grant(id, &mut effects);
                    graph.prune_empty_state(id, &mut effects);
                    wake
                };
                if let Some(waiter) = &wake {
                    remove_marker(&key, waiter);
                }
                (false, wake)
            } else {
                graph.prune_thread(reference.id(), &mut effects);
                ctx.posix_thread.pi_exit().complete();
                (true, None)
            }
        };
        if let Some(waiter) = wake {
            waiter.waker.wake_up();
        }
        effects.flush();
        if complete {
            break;
        }
    }
    ctx.posix_thread.pi_exit().changed.wake_all();
}

pub(crate) fn finish_exec(ctx: &Context) {
    let _graph = graph().lock();
    ctx.posix_thread
        .pi_exit()
        .generation
        .fetch_add(1, Ordering::Release);
    ctx.posix_thread
        .pi_exit()
        .phase
        .store(ExitPhase::Active, Ordering::Release);
}
