// SPDX-License-Identifier: MPL-2.0

use hashbrown::HashMap;
use ostd::{
    cpu::{CpuId, CpuSet},
    sync::Waker,
    task::scheduler::request_preemption,
};

use super::{
    super::{FutexIdentity, FutexKey},
    queue::PriorityQueue,
};
use crate::{
    prelude::*,
    process::{PidIdentity, PidNamespace, posix_thread::AsPosixThread},
    sched::{SchedPolicy, SchedPriority, set_thread_inherited_priority, set_thread_policy},
    thread::Thread,
};

const MAX_PI_CHAIN_DEPTH: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct FutexId(u8, usize, usize);

impl FutexId {
    pub(super) fn from_key(key: &FutexKey) -> Self {
        match &key.identity {
            FutexIdentity::Private { vmar, addr } => Self(0, vmar.as_ptr() as usize, *addr),
            FutexIdentity::SharedLocal { vmar, addr } => Self(1, vmar.as_ptr() as usize, *addr),
            FutexIdentity::Shared { vmo, offset } => Self(2, vmo.as_ptr() as usize, *offset),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct ThreadId(usize);

pub(super) struct ThreadRef {
    pub(super) thread: Arc<Thread>,
    pub(super) identity: Arc<PidIdentity>,
    pub(super) tid: u32,
    pub(super) generation: u64,
}

impl ThreadRef {
    pub(super) fn new(thread: Arc<Thread>, namespace: &PidNamespace) -> Result<Self> {
        let posix = thread
            .as_posix_thread()
            .ok_or_else(|| Error::new(Errno::EPERM))?;
        let identity = posix.pid_identity();
        let generation = posix.pi_exit().generation();
        let tid = identity.in_namespace(namespace);
        if tid == 0 {
            return_errno_with_message!(Errno::ESRCH, "the PI owner is not visible");
        }
        Ok(Self {
            thread,
            identity,
            tid,
            generation,
        })
    }

    pub(super) fn id(&self) -> ThreadId {
        ThreadId(Arc::as_ptr(&self.identity) as usize)
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct WaitOrder {
    priority: SchedPriority,
    sequence: u64,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Donation {
    Waiting(SchedPriority),
    Uncontended,
}

pub(super) struct PiWaiter {
    pub(super) identity: Arc<PidIdentity>,
    pub(super) tid: u32,
    pub(super) waker: Arc<Waker>,
    sequence: u64,
}

impl PiWaiter {
    pub(super) fn id(&self) -> ThreadId {
        ThreadId(Arc::as_ptr(&self.identity) as usize)
    }
}

pub(super) struct Grant {
    pub(super) waiter: Arc<PiWaiter>,
    pub(super) owner_died: bool,
    pub(super) needs_fixup: bool,
    pub(super) previous_owner: Arc<PidIdentity>,
}

pub(super) struct PiState {
    pub(super) key: FutexKey,
    pub(super) owner: ThreadId,
    waiters: PriorityQueue<ThreadId, WaitOrder, Arc<PiWaiter>>,
    pub(super) grant: Option<Grant>,
}

struct PiThread {
    thread: Arc<Thread>,
    identity: Arc<PidIdentity>,
    owned: PriorityQueue<FutexId, Donation, ()>,
    blocked: Option<FutexId>,
    effective: SchedPriority,
    inherited: Option<SchedPriority>,
}

pub(super) struct PiGraph {
    pub(super) states: HashMap<FutexId, PiState>,
    threads: HashMap<ThreadId, PiThread>,
    next_sequence: u64,
}

pub(super) struct Effects {
    cpus: CpuSet,
    retired: [Option<Retired>; 4],
}

enum Retired {
    Thread(PiThread),
    State(PiState),
    Grant(Grant),
}

impl Effects {
    pub(super) fn new() -> Self {
        Self {
            cpus: CpuSet::new_empty(),
            retired: core::array::from_fn(|_| None),
        }
    }

    fn retire(&mut self, resource: Retired) {
        let slot = self
            .retired
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("one PI transition retires at most one grant, state and two threads");
        *slot = Some(resource);
    }

    fn record(&mut self, cpu: Option<CpuId>) {
        if let Some(cpu) = cpu {
            self.cpus.add(cpu);
        }
    }

    pub(super) fn flush(self) {
        let Self { cpus, retired } = self;
        for resource in retired.into_iter().flatten() {
            match resource {
                Retired::Thread(thread) => drop(thread),
                Retired::State(state) => drop(state),
                Retired::Grant(grant) => drop(grant),
            }
        }
        for cpu in cpus.iter() {
            request_preemption(cpu);
        }
    }
}

impl PiThread {
    fn new(reference: &ThreadRef) -> Self {
        Self {
            thread: reference.thread.clone(),
            identity: reference.identity.clone(),
            owned: PriorityQueue::new(),
            blocked: None,
            effective: reference.thread.sched_attr().effective_priority(),
            inherited: None,
        }
    }
}

impl PiGraph {
    pub(super) fn new() -> Self {
        Self {
            states: HashMap::new(),
            threads: HashMap::new(),
            next_sequence: 0,
        }
    }

    pub(super) fn owner_reference(
        &self,
        id: FutexId,
        namespace: &PidNamespace,
    ) -> Result<ThreadRef> {
        let state = self
            .states
            .get(&id)
            .ok_or_else(|| Error::new(Errno::EINVAL))?;
        let owner = self.threads.get(&state.owner).unwrap();
        let tid = owner.identity.in_namespace(namespace);
        if tid == 0 {
            return_errno_with_message!(Errno::ESRCH, "the PI owner is not visible");
        }
        let generation = owner
            .thread
            .as_posix_thread()
            .unwrap()
            .pi_exit()
            .generation();
        Ok(ThreadRef {
            thread: owner.thread.clone(),
            identity: owner.identity.clone(),
            tid,
            generation,
        })
    }

    pub(super) fn prepare(
        &mut self,
        key: &FutexKey,
        current: &ThreadRef,
        owner: &ThreadRef,
    ) -> Result<FutexId> {
        self.check_chain(current.id(), owner.id())?;
        self.threads
            .try_reserve(2)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        self.states
            .try_reserve(1)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        for reference in [current, owner] {
            self.threads
                .entry(reference.id())
                .or_insert_with(|| PiThread::new(reference));
            // Keep a spare insertion for ownership granted while the thread sleeps.
            self.threads
                .get_mut(&reference.id())
                .unwrap()
                .owned
                .reserve(2)?;
        }
        if self.threads.get(&current.id()).unwrap().blocked.is_some() {
            return_errno_with_message!(Errno::EINVAL, "the thread already has a queued PI wait");
        }
        let id = FutexId::from_key(key);
        if let Some(state) = self.states.get_mut(&id) {
            if state.owner != owner.id() {
                return_errno_with_message!(Errno::EINVAL, "the PI word disagrees with its owner");
            }
            state.waiters.reserve(1)?;
        } else {
            let mut waiters = PriorityQueue::new();
            waiters.reserve(1)?;
            self.threads
                .get_mut(&owner.id())
                .unwrap()
                .owned
                .set(id, Donation::Uncontended, ())?;
            self.states.insert(
                id,
                PiState {
                    key: key.clone(),
                    owner: owner.id(),
                    waiters,
                    grant: None,
                },
            );
        }
        Ok(id)
    }

    fn check_chain(&self, current: ThreadId, mut owner: ThreadId) -> Result<()> {
        for _ in 0..MAX_PI_CHAIN_DEPTH {
            if owner == current {
                return_errno_with_message!(Errno::EDEADLK, "the PI wait creates a cycle");
            }
            let Some(blocked) = self.threads.get(&owner).and_then(|owner| owner.blocked) else {
                return Ok(());
            };
            owner = self.states.get(&blocked).unwrap().owner;
        }
        return_errno_with_message!(Errno::EDEADLK, "the PI chain exceeds the maximum depth");
    }

    pub(super) fn new_waiter(
        &mut self,
        current: &ThreadRef,
        waker: Arc<Waker>,
    ) -> Result<Arc<PiWaiter>> {
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| Error::new(Errno::EAGAIN))?;
        Ok(Arc::new(PiWaiter {
            identity: current.identity.clone(),
            tid: current.tid,
            waker,
            sequence,
        }))
    }

    pub(super) fn enqueue(&mut self, id: FutexId, waiter: Arc<PiWaiter>, effects: &mut Effects) {
        let actor = waiter.id();
        let priority = self.threads.get(&actor).unwrap().effective;
        let order = WaitOrder {
            priority,
            sequence: waiter.sequence,
        };
        let state = self.states.get_mut(&id).unwrap();
        state
            .waiters
            .set(actor, order, waiter)
            .expect("PI storage was reserved before publication");
        let owner = state.owner;
        self.threads.get_mut(&actor).unwrap().blocked = Some(id);
        self.refresh_donation(id);
        self.propagate(owner, effects);
    }

    pub(super) fn first_waiter(&self, id: FutexId) -> Option<Arc<PiWaiter>> {
        self.states
            .get(&id)?
            .waiters
            .first()
            .map(|(_, _, waiter)| waiter.clone())
    }

    pub(super) fn has_waiters(&self, id: FutexId) -> bool {
        self.states
            .get(&id)
            .is_some_and(|state| !state.waiters.is_empty())
    }

    fn refresh_donation(&mut self, id: FutexId) {
        let state = self.states.get(&id).unwrap();
        let priority = state
            .waiters
            .first()
            .map_or(Donation::Uncontended, |(_, order, _)| {
                Donation::Waiting(order.priority)
            });
        self.threads
            .get_mut(&state.owner)
            .unwrap()
            .owned
            .set(id, priority, ())
            .expect("owned PI storage was reserved before publication");
    }

    fn propagate(&mut self, mut actor: ThreadId, effects: &mut Effects) {
        for _ in 0..self.threads.len() {
            let record = self.threads.get_mut(&actor).unwrap();
            let inherited = record
                .owned
                .first()
                .and_then(|(_, donation, _)| match donation {
                    Donation::Waiting(priority) => Some(*priority),
                    Donation::Uncontended => None,
                });
            let base = SchedPriority::from_policy(record.thread.sched_attr().policy());
            let effective = inherited.map_or(base, |priority| priority.min(base));
            let previous = core::mem::replace(&mut record.effective, effective);
            if record.inherited != inherited {
                record.inherited = inherited;
                effects.record(set_thread_inherited_priority(&record.thread, inherited));
            }
            if previous == effective {
                return;
            }
            let Some(blocked) = record.blocked else {
                return;
            };
            let state = self.states.get_mut(&blocked).unwrap();
            let waiter = state.waiters.get(&actor).unwrap().clone();
            let order = WaitOrder {
                priority: effective,
                sequence: waiter.sequence,
            };
            state
                .waiters
                .set(actor, order, waiter)
                .expect("priority changes reuse existing PI storage");
            actor = state.owner;
            self.refresh_donation(blocked);
        }
        debug_assert!(false, "a validated PI graph cannot contain a cycle");
    }

    pub(super) fn cancel(&mut self, id: FutexId, waiter: &PiWaiter, effects: &mut Effects) -> bool {
        let Some(state) = self.states.get_mut(&id) else {
            return false;
        };
        if state.waiters.remove(&waiter.id()).is_none() {
            return false;
        }
        let owner = state.owner;
        self.threads.get_mut(&waiter.id()).unwrap().blocked = None;
        self.refresh_donation(id);
        self.propagate(owner, effects);
        self.prune_empty_state(id, effects);
        self.prune_thread(waiter.id(), effects);
        true
    }

    pub(super) fn cancel_thread_wait(
        &mut self,
        actor: ThreadId,
        effects: &mut Effects,
    ) -> Option<(FutexKey, Arc<PiWaiter>)> {
        let id = self.threads.get(&actor)?.blocked?;
        let state = self.states.get(&id).unwrap();
        let key = state.key.clone();
        let waiter = state.waiters.get(&actor).unwrap().clone();
        self.cancel(id, &waiter, effects);
        Some((key, waiter))
    }

    pub(super) fn grant_next(
        &mut self,
        id: FutexId,
        owner_died: bool,
        needs_fixup: bool,
        effects: &mut Effects,
    ) -> Option<Arc<PiWaiter>> {
        let state = self.states.get_mut(&id)?;
        let (_, _, waiter) = state.waiters.pop()?;
        let previous = state.owner;
        let previous_owner = match state.grant.take() {
            Some(grant) => {
                let previous_owner = if grant.needs_fixup {
                    grant.previous_owner.clone()
                } else {
                    self.threads.get(&previous).unwrap().identity.clone()
                };
                effects.retire(Retired::Grant(grant));
                previous_owner
            }
            None => self.threads.get(&previous).unwrap().identity.clone(),
        };
        state.owner = waiter.id();
        state.grant = Some(Grant {
            waiter: waiter.clone(),
            owner_died,
            needs_fixup,
            previous_owner,
        });
        self.threads.get_mut(&previous).unwrap().owned.remove(&id);
        self.threads.get_mut(&waiter.id()).unwrap().blocked = None;
        self.refresh_donation(id);
        self.propagate(previous, effects);
        self.propagate(waiter.id(), effects);
        self.prune_thread(previous, effects);
        Some(waiter)
    }

    pub(super) fn accept_grant(&mut self, id: FutexId, actor: ThreadId, effects: &mut Effects) {
        let state = self.states.get_mut(&id).unwrap();
        debug_assert!(
            state
                .grant
                .as_ref()
                .is_some_and(|grant| grant.waiter.id() == actor)
        );
        effects.retire(Retired::Grant(state.grant.take().unwrap()));
        self.prune_empty_state(id, effects);
    }

    pub(super) fn first_owned(&self, actor: ThreadId) -> Option<FutexId> {
        self.threads
            .get(&actor)?
            .owned
            .first()
            .map(|(key, _, _)| *key)
    }

    pub(super) fn abandon_grant(
        &mut self,
        id: FutexId,
        effects: &mut Effects,
    ) -> Option<Arc<PiWaiter>> {
        let state = self.states.get(&id)?;
        let owner_died = state.grant.as_ref()?.owner_died;
        if state.waiters.is_empty() {
            let grant = self.states.get_mut(&id).unwrap().grant.take().unwrap();
            effects.retire(Retired::Grant(grant));
            self.prune_empty_state(id, effects);
            None
        } else {
            self.grant_next(id, owner_died, true, effects)
        }
    }

    pub(super) fn prune_empty_state(&mut self, id: FutexId, effects: &mut Effects) {
        let Some(state) = self.states.get(&id) else {
            return;
        };
        if !state.waiters.is_empty() || state.grant.is_some() {
            return;
        }
        let owner = state.owner;
        effects.retire(Retired::State(self.states.remove(&id).unwrap()));
        self.threads.get_mut(&owner).unwrap().owned.remove(&id);
        self.propagate(owner, effects);
        self.prune_thread(owner, effects);
    }

    pub(super) fn prune_thread(&mut self, actor: ThreadId, effects: &mut Effects) {
        if self
            .threads
            .get(&actor)
            .is_some_and(|thread| thread.blocked.is_none() && thread.owned.is_empty())
        {
            effects.retire(Retired::Thread(self.threads.remove(&actor).unwrap()));
        }
    }

    pub(super) fn update_policy(
        &mut self,
        thread: &ThreadRef,
        update: impl FnOnce(SchedPolicy) -> Result<SchedPolicy>,
        effects: &mut Effects,
    ) -> Result<()> {
        let policy = update(thread.thread.sched_attr().policy())?;
        effects.record(set_thread_policy(&thread.thread, policy));
        if self.threads.contains_key(&thread.id()) {
            self.propagate(thread.id(), effects);
        }
        Ok(())
    }
}
