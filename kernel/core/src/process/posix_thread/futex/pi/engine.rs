// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::{Ordering, fence};

use ostd::sync::Waiter;

use super::{
    super::{
        FUTEX_BITSET_MATCH_ANY, FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, FutexIdentity,
        FutexItem, FutexKey, FutexVisibility, FutexWaitKind, get_futex_bucket,
    },
    ExitPhase, graph, remove_marker,
    state::{Effects, FutexId, PiGraph, PiWaiter, ThreadRef},
};
use crate::{
    prelude::*,
    process::{pid_table, posix_thread::AsPosixThread},
    thread::Thread,
    time::wait::ManagedTimeout,
    vm::{perms::VmPerms, vmar::PageFaultInfo},
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum PiLockMode {
    Wait,
    Try,
}

struct OwnerLookup {
    key: FutexId,
    tid: u32,
    owner: Option<ThreadRef>,
    compatible: bool,
}

struct LockRequest<'a> {
    address: Vaddr,
    key: &'a FutexKey,
    actor: &'a ThreadRef,
    lookup: Option<&'a OwnerLookup>,
    waiter: Option<&'a Waiter>,
    mode: PiLockMode,
    ctx: &'a Context<'a>,
}

enum WordAttempt {
    Complete,
    Retry,
    Fault,
}

enum Attempt {
    Word(WordAttempt),
    Lookup(u32),
    CreateWaiter,
    OwnerExiting(Arc<Thread>),
    Queued(Arc<PiWaiter>),
}

fn resolve_fault(address: Vaddr, ctx: &Context) -> Result<()> {
    ctx.user_space()
        .vmar()
        .handle_page_fault(&PageFaultInfo::new(address, VmPerms::READ | VmPerms::WRITE))
        .map_err(|_| Error::new(Errno::EFAULT))
}

fn load_word(address: Vaddr, ctx: &Context) -> Option<Result<u32>> {
    ctx.thread_local
        .with_page_fault_disabled(|| ctx.user_space().atomic_load(address))
}

fn exchange_word(address: Vaddr, old: u32, new: u32, ctx: &Context) -> Result<WordAttempt> {
    match ctx
        .thread_local
        .with_page_fault_disabled(|| {
            let user_space = ctx.user_space();
            let (reader, writer) = user_space.reader_writer(address, size_of::<u32>())?;
            Ok(writer.atomic_compare_exchange(&reader, old, new)?.1)
        })
        .transpose()?
    {
        Some(true) => Ok(WordAttempt::Complete),
        Some(false) => Ok(WordAttempt::Retry),
        None => Ok(WordAttempt::Fault),
    }
}

fn key_at(address: Vaddr, visibility: FutexVisibility, ctx: &Context) -> Result<FutexKey> {
    FutexKey::new(
        address,
        FUTEX_BITSET_MATCH_ANY,
        &ctx.user_space(),
        visibility,
    )
}

fn lookup_owner(tid: u32, key: &FutexKey, ctx: &Context) -> Result<OwnerLookup> {
    let thread = {
        let table = pid_table::pid_table_mut();
        table.get_thread_in(tid, ctx.process.pid_namespace())
    };
    let Some(thread) = thread else {
        return Ok(OwnerLookup {
            key: FutexId::from_key(key),
            tid,
            owner: None,
            compatible: false,
        });
    };
    let reference = ThreadRef::new(thread, ctx.process.pid_namespace())?;
    let compatible = if let FutexIdentity::Private { vmar, .. } = &key.identity {
        reference
            .thread
            .as_posix_thread()
            .unwrap()
            .weak_process()
            .upgrade()
            .is_some_and(|process| process.lock_vmar().is_same_as(vmar))
    } else {
        true
    };
    Ok(OwnerLookup {
        key: FutexId::from_key(key),
        tid,
        owner: Some(reference),
        compatible,
    })
}

fn attempt_lock(
    request: &LockRequest,
    graph: &mut PiGraph,
    effects: &mut Effects,
) -> Result<Attempt> {
    let LockRequest {
        address,
        key,
        actor,
        lookup,
        waiter,
        mode,
        ctx,
    } = *request;
    let id = FutexId::from_key(key);
    let mut bucket = get_futex_bucket(key).1.lock();
    let Some(word) = load_word(address, ctx) else {
        return Ok(Attempt::Word(WordAttempt::Fault));
    };
    let word = word?;
    let tid = word & FUTEX_TID_MASK;
    if tid == actor.tid {
        return_errno_with_message!(
            Errno::EDEADLK,
            "the PI futex is already owned by the caller"
        );
    }
    if bucket
        .items
        .iter()
        .any(|item| item.key.match_up(key) && item.kind == FutexWaitKind::Ordinary)
    {
        return_errno_with_message!(Errno::EINVAL, "ordinary waiters cannot share a PI queue");
    }
    if !graph.states.contains_key(&id) && tid == 0 {
        let value = actor.tid | (word & FUTEX_OWNER_DIED);
        return exchange_word(address, word, value, ctx).map(Attempt::Word);
    }
    let known;
    let (owner, compatible, valid) = if graph.states.contains_key(&id) {
        known = graph.owner_reference(id, ctx.process.pid_namespace())?;
        let state = graph.states.get(&id).unwrap();
        let valid = if let Some(grant) = &state.grant {
            if grant.needs_fixup {
                tid == grant
                    .previous_owner
                    .in_namespace(ctx.process.pid_namespace())
                    || tid == known.tid
                    || (tid == 0 && word & FUTEX_OWNER_DIED != 0)
            } else {
                tid == known.tid
            }
        } else {
            tid == known.tid
        };
        (Some(&known), true, valid)
    } else {
        let Some(lookup) = lookup.filter(|lookup| lookup.key == id && lookup.tid == tid) else {
            return Ok(Attempt::Lookup(tid));
        };
        (lookup.owner.as_ref(), lookup.compatible, true)
    };
    if let Some(owner) = owner {
        let exit = owner.thread.as_posix_thread().unwrap().pi_exit();
        if exit.generation() != owner.generation {
            return Ok(Attempt::Lookup(tid));
        }
        match exit.phase() {
            ExitPhase::Exiting => return Ok(Attempt::OwnerExiting(owner.thread.clone())),
            ExitPhase::Exited => {
                return_errno_with_message!(Errno::ESRCH, "the PI owner has exited")
            }
            ExitPhase::Active => {}
        }
        if !valid {
            return_errno_with_message!(
                Errno::EINVAL,
                "the PI word disagrees with kernel ownership"
            );
        }
        if !compatible {
            return_errno_with_message!(
                Errno::EPERM,
                "the private PI owner has another address space"
            );
        }
        if owner.id() == actor.id() {
            return_errno_with_message!(Errno::EDEADLK, "the PI wait creates a cycle");
        }
    }
    if owner.is_some() && mode == PiLockMode::Wait && waiter.is_none() {
        return Ok(Attempt::CreateWaiter);
    }
    let prepared = if let Some(owner) = owner.filter(|_| mode == PiLockMode::Wait) {
        graph.prepare(key, actor, owner)?;
        bucket
            .items
            .try_reserve(1)
            .map_err(|_| Error::new(Errno::ENOMEM))?;
        Some(graph.new_waiter(actor, waiter.unwrap().waker())?)
    } else {
        None
    };
    if word & FUTEX_WAITERS == 0 {
        match exchange_word(address, word, word | FUTEX_WAITERS, ctx)? {
            WordAttempt::Complete => {}
            attempt => return Ok(Attempt::Word(attempt)),
        }
    }
    if owner.is_none() {
        return_errno_with_message!(Errno::ESRCH, "the PI owner does not exist");
    }
    if mode == PiLockMode::Try {
        return_errno_with_message!(Errno::EAGAIN, "the PI futex is owned");
    }
    let pending = prepared.unwrap();
    bucket.items.push(FutexItem {
        key: key.clone(),
        waker: pending.waker.clone(),
        kind: FutexWaitKind::PriorityInheritance,
    });
    graph.enqueue(id, pending.clone(), effects);
    Ok(Attempt::Queued(pending))
}

pub(crate) fn lock_pi(
    address: Vaddr,
    mode: PiLockMode,
    timeout: Option<ManagedTimeout>,
    visibility: FutexVisibility,
    ctx: &Context,
) -> Result<()> {
    let actor = ThreadRef::new(Thread::current().unwrap(), ctx.process.pid_namespace())?;
    let mut lookup = None;
    let mut sleeper = None;
    loop {
        let key = key_at(address, visibility, ctx)?;
        let id = FutexId::from_key(&key);
        let mut effects = Effects::new();
        let attempt = {
            let mut graph = graph().lock();
            let request = LockRequest {
                address,
                key: &key,
                actor: &actor,
                lookup: lookup.as_ref(),
                waiter: sleeper.as_ref(),
                mode,
                ctx,
            };
            let result = attempt_lock(&request, &mut graph, &mut effects);
            if !matches!(&result, Ok(Attempt::Queued(_))) {
                graph.prune_empty_state(id, &mut effects);
                graph.prune_thread(actor.id(), &mut effects);
                if let Some(owner) = lookup.as_ref().and_then(|lookup| lookup.owner.as_ref()) {
                    graph.prune_thread(owner.id(), &mut effects);
                }
            }
            result
        };
        effects.flush();
        match attempt? {
            Attempt::Word(WordAttempt::Complete) => {
                fence(Ordering::Acquire);
                return Ok(());
            }
            Attempt::Word(WordAttempt::Retry) => {}
            Attempt::Word(WordAttempt::Fault) => resolve_fault(address, ctx)?,
            Attempt::Lookup(tid) => lookup = Some(lookup_owner(tid, &key, ctx)?),
            Attempt::CreateWaiter => sleeper = Some(Waiter::new_pair().0),
            Attempt::OwnerExiting(owner) => owner.as_posix_thread().unwrap().pi_exit().wait()?,
            Attempt::Queued(waiter) => {
                return wait_for_grant(
                    address,
                    &key,
                    waiter,
                    sleeper.as_ref().unwrap(),
                    timeout,
                    visibility,
                    ctx,
                );
            }
        }
    }
}

fn wait_for_grant(
    address: Vaddr,
    key: &FutexKey,
    waiter: Arc<PiWaiter>,
    sleeper: &Waiter,
    timeout: Option<ManagedTimeout>,
    visibility: FutexVisibility,
    ctx: &Context,
) -> Result<()> {
    let id = FutexId::from_key(key);
    let paused = sleeper.pause_until_or_timeout(
        || {
            graph()
                .lock()
                .states
                .get(&id)
                .and_then(|state| state.grant.as_ref())
                .filter(|grant| Arc::ptr_eq(&grant.waiter, &waiter))
                .map(|_| ())
        },
        timeout,
    );
    let mut effects = Effects::new();
    let granted = {
        let mut graph = graph().lock();
        let granted = graph
            .states
            .get(&id)
            .and_then(|state| state.grant.as_ref())
            .is_some_and(|grant| Arc::ptr_eq(&grant.waiter, &waiter));
        if !granted {
            graph.cancel(id, &waiter, &mut effects);
            remove_marker(key, &waiter);
        }
        granted
    };
    effects.flush();
    if !granted {
        return paused.and(Err(Error::new(Errno::EINTR)));
    }
    finish_grant(address, key, &waiter, visibility, ctx)
}

fn finish_grant(
    address: Vaddr,
    key: &FutexKey,
    waiter: &PiWaiter,
    visibility: FutexVisibility,
    ctx: &Context,
) -> Result<()> {
    let id = FutexId::from_key(key);
    let result = loop {
        let mapping = key_at(address, visibility, ctx);
        match mapping {
            Ok(current) if FutexId::from_key(&current) == id => {}
            Ok(_) => break Err(Error::new(Errno::EFAULT)),
            Err(error) => break Err(error),
        }
        let mut effects = Effects::new();
        let attempt = {
            let mut graph = graph().lock();
            (|| {
                let state = graph
                    .states
                    .get(&id)
                    .ok_or_else(|| Error::new(Errno::EINVAL))?;
                let grant = state
                    .grant
                    .as_ref()
                    .ok_or_else(|| Error::new(Errno::EINVAL))?;
                if !core::ptr::eq(Arc::as_ptr(&grant.waiter), waiter) {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "the PI grant belongs to another waiter"
                    );
                }
                let Some(word) = load_word(address, ctx) else {
                    return Ok(WordAttempt::Fault);
                };
                let word = word?;
                let tid = word & FUTEX_TID_MASK;
                if grant.needs_fixup {
                    let previous = grant
                        .previous_owner
                        .in_namespace(ctx.process.pid_namespace());
                    if tid != previous
                        && tid != waiter.tid
                        && !(tid == 0 && word & FUTEX_OWNER_DIED != 0)
                    {
                        return_errno_with_message!(
                            Errno::EINVAL,
                            "the PI word was changed during handoff"
                        );
                    }
                    let value = waiter.tid
                        | FUTEX_WAITERS
                        | (word & FUTEX_OWNER_DIED)
                        | if grant.owner_died {
                            FUTEX_OWNER_DIED
                        } else {
                            0
                        };
                    match exchange_word(address, word, value, ctx)? {
                        WordAttempt::Complete => {}
                        attempt => return Ok(attempt),
                    }
                } else if tid != waiter.tid {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "the PI word was changed during handoff"
                    );
                }
                graph.accept_grant(id, waiter.id(), &mut effects);
                Ok(WordAttempt::Complete)
            })()
        };
        effects.flush();
        match attempt {
            Ok(WordAttempt::Complete) => {
                fence(Ordering::Acquire);
                break Ok(());
            }
            Ok(WordAttempt::Retry) => {}
            Ok(WordAttempt::Fault) => {
                if let Err(error) = resolve_fault(address, ctx) {
                    break Err(error);
                }
            }
            Err(error) => break Err(error),
        }
    };
    if result.is_err() {
        let mut effects = Effects::new();
        let wake = {
            let mut graph = graph().lock();
            let wake = graph.abandon_grant(id, &mut effects);
            if let Some(next) = &wake {
                remove_marker(key, next);
            }
            wake
        };
        if let Some(next) = wake {
            next.waker.wake_up();
        }
        effects.flush();
    }
    result
}

pub(crate) fn unlock_pi(address: Vaddr, visibility: FutexVisibility, ctx: &Context) -> Result<()> {
    let actor = ThreadRef::new(Thread::current().unwrap(), ctx.process.pid_namespace())?;
    loop {
        let key = key_at(address, visibility, ctx)?;
        let id = FutexId::from_key(&key);
        let mut effects = Effects::new();
        let mut wake = None;
        let attempt = {
            let mut graph = graph().lock();
            let mut bucket = get_futex_bucket(&key).1.lock();
            (|| {
                let Some(word) = load_word(address, ctx) else {
                    return Ok(WordAttempt::Fault);
                };
                let word = word?;
                if word & FUTEX_TID_MASK != actor.tid {
                    return_errno_with_message!(
                        Errno::EPERM,
                        "the caller does not own the PI futex"
                    );
                }
                if bucket
                    .items
                    .iter()
                    .any(|item| item.key.match_up(&key) && item.kind == FutexWaitKind::Ordinary)
                {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "ordinary waiters cannot share a PI queue"
                    );
                }
                if let Some(state) = graph.states.get(&id)
                    && (state.owner != actor.id() || state.grant.is_some())
                {
                    return_errno_with_message!(
                        Errno::EINVAL,
                        "the PI word disagrees with kernel ownership"
                    );
                }
                let next = graph.first_waiter(id);
                let value = next.as_ref().map_or(0, |waiter| waiter.tid | FUTEX_WAITERS);
                fence(Ordering::Release);
                match exchange_word(address, word, value, ctx)? {
                    WordAttempt::Complete => {}
                    attempt => return Ok(attempt),
                }
                if next.is_some() {
                    wake = graph.grant_next(id, false, false, &mut effects);
                    let next = wake.as_ref().unwrap();
                    bucket.remove_by_waker(&next.waker);
                } else {
                    graph.prune_empty_state(id, &mut effects);
                }
                Ok(WordAttempt::Complete)
            })()
        };
        if let Some(waiter) = wake {
            waiter.waker.wake_up();
        }
        effects.flush();
        match attempt? {
            WordAttempt::Complete => return Ok(()),
            WordAttempt::Retry => {}
            WordAttempt::Fault => resolve_fault(address, ctx)?,
        }
    }
}
