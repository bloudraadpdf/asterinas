// SPDX-License-Identifier: MPL-2.0

use ostd::sync::WaitQueue;
use sparse_id_alloc::SparseIdAlloc;
use spin::Once;

use crate::{
    fs::pseudofs::{NsCommonOps, NsType, StashedDentry},
    prelude::*,
    process::{
        Pid, Process, UserNamespace,
        credentials::capabilities::CapSet,
        posix_thread::{PID_MAX, PosixThread},
    },
};

/// A hierarchical process identifier namespace.
pub(crate) struct PidNamespace {
    parent: Option<Arc<Self>>,
    depth: usize,
    owner: Arc<UserNamespace>,
    stashed_dentry: StashedDentry,
    inner: SpinLock<Inner>,
    admission_wait_queue: WaitQueue,
}

struct Inner {
    ids: SparseIdAlloc,
    next_root_pid: Pid,
    last_pid: Pid,
    members: BTreeMap<Pid, Weak<PidIdentity>>,
    state: State,
    admitted: usize,
}

enum State {
    Root,
    New,
    Starting,
    Running(Weak<Process>),
    Closing(Weak<Process>),
    Aborted,
}

impl PidNamespace {
    pub(crate) fn get_init_singleton() -> &'static Arc<Self> {
        static INIT: Once<Arc<PidNamespace>> = Once::new();
        INIT.call_once(|| Self::new(None, UserNamespace::get_init_singleton().clone()))
    }

    fn new(parent: Option<Arc<Self>>, owner: Arc<UserNamespace>) -> Arc<Self> {
        let depth = parent.as_ref().map_or(0, |parent| parent.depth + 1);
        let state = if parent.is_some() {
            State::New
        } else {
            State::Root
        };
        Arc::new(Self {
            parent,
            depth,
            owner,
            stashed_dentry: StashedDentry::new(),
            inner: SpinLock::new(Inner {
                ids: SparseIdAlloc::new(1, PID_MAX - 1),
                next_root_pid: 1,
                last_pid: 0,
                members: BTreeMap::new(),
                state,
                admitted: 0,
            }),
            admission_wait_queue: WaitQueue::new(),
        })
    }

    pub(crate) fn new_clone(
        self: &Arc<Self>,
        owner: Arc<UserNamespace>,
        posix_thread: &PosixThread,
    ) -> Result<Arc<Self>> {
        owner.check_capability(posix_thread, CapSet::SYS_ADMIN)?;
        if self.depth == 32 {
            return_errno_with_message!(Errno::ENOSPC, "PID namespace nesting limit reached");
        }
        Ok(Self::new(Some(self.clone()), owner))
    }

    pub(crate) fn is_same_or_ancestor_of(self: &Arc<Self>, other: &Arc<Self>) -> bool {
        let mut namespace = other;
        while namespace.depth > self.depth {
            namespace = namespace.parent.as_ref().unwrap();
        }
        Arc::ptr_eq(self, namespace)
    }

    /// Resolves an identifier without changing the caller's namespace.
    pub(crate) fn resolve(&self, pid: Pid) -> Option<Pid> {
        if self.depth == 0 {
            return Some(pid);
        }
        self.inner
            .lock()
            .members
            .get(&pid)?
            .upgrade()
            .map(|identity| identity.global())
    }

    pub(crate) fn child_reaper(&self) -> Option<Arc<Process>> {
        match &self.inner.lock().state {
            State::Running(init) | State::Closing(init) => init.upgrade(),
            _ => None,
        }
    }

    pub(crate) fn last_pid(&self) -> Pid {
        self.inner.lock().last_pid
    }

    pub(crate) fn has_init(&self) -> bool {
        matches!(
            self.inner.lock().state,
            State::Root | State::Running(_) | State::Closing(_)
        )
    }

    fn reserve(self: &Arc<Self>) -> Result<(PidNumber, Option<SpawnPermit>)> {
        self.admission_wait_queue.wait_until(|| {
            let mut inner = self.inner.lock();
            let is_init = match inner.state {
                State::New => true,
                State::Starting => return None,
                State::Closing(_) | State::Aborted => {
                    return Some(Err(Error::with_message(
                        Errno::ENOMEM,
                        "the PID namespace init has exited",
                    )));
                }
                State::Root | State::Running(_) => false,
            };
            let id = if self.depth == 0 {
                let id = inner.next_root_pid;
                (id < PID_MAX).then(|| {
                    inner.next_root_pid += 1;
                    id
                })
            } else {
                inner.ids.alloc()
            };
            let Some(id) = id else {
                return Some(Err(Error::with_message(
                    Errno::EAGAIN,
                    "no PID is available",
                )));
            };
            inner.last_pid = id;
            if is_init {
                debug_assert_eq!(id, 1);
                inner.state = State::Starting;
            }
            let permit = (self.depth != 0).then(|| {
                inner.admitted += 1;
                SpawnPermit {
                    namespace: self.clone(),
                    is_init,
                }
            });
            Some(Ok((
                PidNumber {
                    namespace: self.clone(),
                    id,
                },
                permit,
            )))
        })
    }

    pub(crate) fn allocate(self: &Arc<Self>) -> Result<PidAllocation> {
        let mut ancestry = Vec::with_capacity(self.depth + 1);
        let mut next = Some(self);
        while let Some(namespace) = next {
            ancestry.push(namespace);
            next = namespace.parent.as_ref();
        }

        let mut numbers = Vec::with_capacity(ancestry.len());
        let mut permits = Vec::with_capacity(self.depth);
        for namespace in ancestry {
            let (number, permit) = namespace.reserve()?;
            numbers.push(number);
            if let Some(permit) = permit {
                permits.push(permit);
            }
        }
        numbers.reverse();

        let identity = Arc::new_cyclic(|weak_identity| {
            for number in &numbers {
                if number.namespace.depth == 0 {
                    continue;
                }
                number
                    .namespace
                    .inner
                    .lock()
                    .members
                    .insert(number.id, weak_identity.clone());
            }
            PidIdentity { numbers }
        });
        Ok(PidAllocation { identity, permits })
    }

    /// Closes admission and returns identities that cannot be reused during teardown.
    pub(crate) fn close(&self) -> Vec<Arc<PidIdentity>> {
        {
            let mut inner = self.inner.lock();
            let State::Running(init) = &inner.state else {
                panic!("only a running PID namespace can close");
            };
            inner.state = State::Closing(init.clone());
        }
        self.admission_wait_queue.wake_all();
        self.admission_wait_queue
            .wait_until(|| (self.inner.lock().admitted == 0).then_some(()));
        self.inner
            .lock()
            .members
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }

    pub(crate) fn wait_for_exit(&self, processes: &[Arc<Process>]) {
        self.admission_wait_queue.wait_until(|| {
            processes
                .iter()
                .all(|process| process.status().is_zombie())
                .then_some(())
        });
    }

    pub(crate) fn notify_exit(&self) {
        self.admission_wait_queue.wake_all();
        if let Some(parent) = &self.parent {
            parent.notify_exit();
        }
    }
}

impl NsCommonOps for PidNamespace {
    const TYPE: NsType = NsType::Pid;

    fn owner_user_ns(&self) -> Option<&Arc<UserNamespace>> {
        Some(&self.owner)
    }

    fn parent(&self) -> Result<&Arc<Self>> {
        self.parent.as_ref().ok_or_else(|| {
            Error::with_message(Errno::EPERM, "the root PID namespace has no parent")
        })
    }

    fn stashed_dentry(&self) -> &StashedDentry {
        &self.stashed_dentry
    }
}

/// The stable identity shared by a thread, process, process group or session.
pub(crate) struct PidIdentity {
    numbers: Vec<PidNumber>,
}

impl PidIdentity {
    pub(crate) fn global(&self) -> Pid {
        self.numbers[0].id
    }

    pub(crate) fn namespace(&self) -> &Arc<PidNamespace> {
        &self.numbers.last().unwrap().namespace
    }

    pub(crate) fn in_namespace(&self, namespace: &PidNamespace) -> Pid {
        self.numbers
            .get(namespace.depth)
            .filter(|number| core::ptr::eq(number.namespace.as_ref(), namespace))
            .map_or(0, |number| number.id)
    }

    pub(crate) fn namespace_ids_from(&self, namespace: &PidNamespace) -> Vec<Pid> {
        if self.in_namespace(namespace) == 0 {
            return vec![0];
        }
        self.numbers[namespace.depth..]
            .iter()
            .map(|number| number.id)
            .collect()
    }
}

impl Debug for PidIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list()
            .entries(self.numbers.iter().map(|number| number.id))
            .finish()
    }
}

struct PidNumber {
    namespace: Arc<PidNamespace>,
    id: Pid,
}

impl Drop for PidNumber {
    fn drop(&mut self) {
        if self.namespace.depth == 0 {
            return;
        }
        {
            let mut inner = self.namespace.inner.lock();
            inner.members.remove(&self.id);
            inner.ids.free(self.id);
            if self.id == 1 && matches!(inner.state, State::Starting) {
                inner.state = State::Aborted;
            }
        }
        self.namespace.admission_wait_queue.wake_all();
    }
}

/// Reserves namespace admission until publication or rollback.
pub(crate) struct PidAllocation {
    identity: Arc<PidIdentity>,
    permits: Vec<SpawnPermit>,
}

impl PidAllocation {
    pub(crate) fn identity(&self) -> &Arc<PidIdentity> {
        &self.identity
    }

    pub(crate) fn publish(self, process: &Arc<Process>) {
        for permit in &self.permits {
            if permit.is_init {
                let mut inner = permit.namespace.inner.lock();
                debug_assert!(matches!(inner.state, State::Starting));
                debug_assert!(Arc::ptr_eq(process.pid_identity(), &self.identity));
                inner.state = State::Running(Arc::downgrade(process));
            }
        }
    }
}

struct SpawnPermit {
    namespace: Arc<PidNamespace>,
    is_init: bool,
}

impl Drop for SpawnPermit {
    fn drop(&mut self) {
        {
            let mut inner = self.namespace.inner.lock();
            inner.admitted -= 1;
        }
        self.namespace.admission_wait_queue.wake_all();
    }
}
