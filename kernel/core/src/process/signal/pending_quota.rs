// SPDX-License-Identifier: MPL-2.0

use hashbrown::HashMap;
use spin::Once;

use crate::{
    prelude::*,
    process::{Process, UserNamespace, posix_thread::PosixThread, rlimit::ResourceType},
};

type UserKey = (u32, usize);

static PENDING: Once<Mutex<HashMap<UserKey, usize>>> = Once::new();

fn pending() -> &'static Mutex<HashMap<UserKey, usize>> {
    PENDING.call_once(|| Mutex::new(HashMap::new()))
}

pub(crate) struct PendingScope {
    uid: u32,
    namespace: Weak<UserNamespace>,
    limit: usize,
}

impl PendingScope {
    pub(crate) fn new(process: &Process, thread: &PosixThread) -> Self {
        Self {
            uid: thread.credentials().ruid().into(),
            namespace: Arc::downgrade(&process.user_ns().lock()),
            limit: process
                .resource_limits()
                .get_rlimit(ResourceType::RLIMIT_SIGPENDING)
                .get_cur()
                .min(usize::MAX as u64) as usize,
        }
    }

    pub(super) fn charge(self) -> Result<PendingCharge> {
        let key = (self.uid, self.namespace.as_ptr() as usize);
        let mut pending = pending().lock();
        let count = pending.get(&key).copied().unwrap_or(0);
        let next = count
            .checked_add(1)
            .filter(|next| *next <= self.limit)
            .ok_or_else(|| Error::new(Errno::EAGAIN))?;
        if count == 0 {
            pending
                .try_reserve(1)
                .map_err(|_| Error::new(Errno::EAGAIN))?;
        }
        pending.insert(key, next);
        Ok(PendingCharge {
            uid: self.uid,
            namespace: self.namespace,
        })
    }
}

pub(super) struct PendingCharge {
    uid: u32,
    namespace: Weak<UserNamespace>,
}

impl Drop for PendingCharge {
    fn drop(&mut self) {
        let key = (self.uid, self.namespace.as_ptr() as usize);
        let mut pending = pending().lock();
        let count = pending
            .get_mut(&key)
            .expect("a pending signal owns its user charge");
        *count -= 1;
        if *count == 0 {
            pending.remove(&key);
        }
    }
}
