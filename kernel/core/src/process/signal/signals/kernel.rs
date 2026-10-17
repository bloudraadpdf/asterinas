// SPDX-License-Identifier: MPL-2.0

use super::Signal;
use crate::process::signal::{
    c_types::siginfo_t,
    constants::{SI_KERNEL, SIGKILL},
    sig_num::SigNum,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KernelSignal {
    num: SigNum,
    forced: bool,
}

impl KernelSignal {
    pub(crate) const fn new(num: SigNum) -> Self {
        Self { num, forced: false }
    }

    /// Creates a kernel-enforced termination signal.
    pub(crate) const fn new_kill() -> Self {
        Self {
            num: SIGKILL,
            forced: true,
        }
    }
}

impl Signal for KernelSignal {
    fn num(&self) -> SigNum {
        self.num
    }

    fn to_info(&self, _namespace: &crate::process::PidNamespace) -> siginfo_t {
        siginfo_t::new(self.num, SI_KERNEL)
    }

    fn forces_namespace_init(
        &self,
        _namespace: &alloc::sync::Arc<crate::process::PidNamespace>,
    ) -> bool {
        self.forced
    }
}
