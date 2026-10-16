// SPDX-License-Identifier: MPL-2.0

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::{
    cpu::{AtomicCpuSet, CpuId, CpuSet, PinCurrentCpu},
    prelude::*,
    sync::{SpinLock, WaitQueue},
    task::atomic_mode::AsAtomicModeGuard,
};

/// A RCU monitor ensures the completion of _grace periods_ by keeping track
/// of each CPU's passing _quiescent states_.
pub(super) struct RcuMonitor {
    is_monitoring: AtomicBool,
    state: SpinLock<State>,
    callbacks_done: WaitQueue,
}

impl RcuMonitor {
    /// Creates a new RCU monitor.
    ///
    /// This function is used to initialize a singleton instance of `RcuMonitor`.
    /// The singleton instance is globally accessible via the `RCU_MONITOR`.
    pub(super) fn new() -> Self {
        Self {
            is_monitoring: AtomicBool::new(false),
            state: SpinLock::new(State::new()),
            callbacks_done: WaitQueue::new(),
        }
    }

    pub(super) unsafe fn finish_grace_period(&self) {
        // Fast path
        if !self.is_monitoring.load(Relaxed) {
            return;
        }

        // Check if the current GP is complete after passing the quiescent state
        // on the current CPU. If GP is complete, take the callbacks of the current
        // GP.
        let callbacks = {
            let mut state = self.state.disable_irq().lock();
            let cpu = state.as_atomic_mode_guard().current_cpu();
            if state.current_gp.is_complete() {
                return;
            }

            state.current_gp.finish_grace_period(cpu);
            if !state.current_gp.is_complete() {
                return;
            }

            // Now that the current GP is complete, take its callbacks
            let current_callbacks = state.current_gp.take_callbacks();

            // Check if we need to watch for a next GP
            if !state.next_callbacks.is_empty() {
                let callbacks = core::mem::take(&mut state.next_callbacks);
                state.current_gp.restart(callbacks);
            } else {
                self.is_monitoring.store(false, Relaxed);
            }

            current_callbacks
        };

        // Invoke the callbacks to notify the completion of GP
        for f in callbacks {
            (f)();
        }
    }

    pub(super) fn after_grace_period<F>(&'static self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let mut state = self.state.disable_irq().lock();

        state.pending_callbacks = state.pending_callbacks.checked_add(1).unwrap();
        state.next_callbacks.push_back(Box::new(move || {
            f();
            let completed = {
                let mut state = self.state.disable_irq().lock();
                state.pending_callbacks -= 1;
                state.pending_callbacks == 0
            };
            if completed {
                self.callbacks_done.wake_all();
            }
        }));

        if !state.current_gp.is_complete() {
            return;
        }

        let callbacks = core::mem::take(&mut state.next_callbacks);
        state.current_gp.restart(callbacks);
        self.is_monitoring.store(true, Relaxed);
    }

    pub(super) fn barrier(&self) {
        self.callbacks_done
            .wait_until(|| (self.state.disable_irq().lock().pending_callbacks == 0).then_some(()));
    }
}

struct State {
    current_gp: GracePeriod,
    next_callbacks: Callbacks,
    pending_callbacks: usize,
}

impl State {
    fn new() -> Self {
        Self {
            current_gp: GracePeriod::new(),
            next_callbacks: VecDeque::new(),
            pending_callbacks: 0,
        }
    }
}

type Callbacks = VecDeque<Box<dyn FnOnce() + Send + 'static>>;

struct GracePeriod {
    callbacks: Callbacks,
    cpu_mask: AtomicCpuSet,
    is_complete: bool,
}

impl GracePeriod {
    fn new() -> Self {
        Self {
            callbacks: Callbacks::new(),
            cpu_mask: AtomicCpuSet::new(CpuSet::new_empty()),
            is_complete: true,
        }
    }

    fn is_complete(&self) -> bool {
        self.is_complete
    }

    fn finish_grace_period(&mut self, this_cpu: CpuId) {
        self.cpu_mask.add(this_cpu, Relaxed);

        if self.cpu_mask.load(Relaxed).is_full() {
            self.is_complete = true;
        }
    }

    fn take_callbacks(&mut self) -> Callbacks {
        core::mem::take(&mut self.callbacks)
    }

    fn restart(&mut self, callbacks: Callbacks) {
        self.is_complete = false;
        self.cpu_mask.store(&CpuSet::new_empty(), Relaxed);
        self.callbacks = callbacks;
    }
}

#[cfg(ktest)]
mod tests {
    use super::*;
    use crate::prelude::ktest;

    #[ktest]
    fn barrier_counts_running_and_nested_callbacks() {
        let monitor = Box::leak(Box::new(RcuMonitor::new()));
        let monitor: &'static RcuMonitor = monitor;
        monitor.after_grace_period(move || {
            assert_eq!(monitor.state.disable_irq().lock().pending_callbacks, 1);
            monitor.after_grace_period(|| {});
            assert_eq!(monitor.state.disable_irq().lock().pending_callbacks, 2);
        });
        let first = monitor
            .state
            .disable_irq()
            .lock()
            .current_gp
            .take_callbacks();
        assert_eq!(monitor.state.disable_irq().lock().pending_callbacks, 1);
        for callback in first {
            callback();
        }
        assert_eq!(monitor.state.disable_irq().lock().pending_callbacks, 1);
        let nested = core::mem::take(&mut monitor.state.disable_irq().lock().next_callbacks);
        for callback in nested {
            callback();
        }
        assert_eq!(monitor.state.disable_irq().lock().pending_callbacks, 0);
        monitor.barrier();
    }
}
