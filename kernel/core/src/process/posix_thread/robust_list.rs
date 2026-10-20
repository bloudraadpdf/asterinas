// SPDX-License-Identifier: MPL-2.0

//! The implementation of robust list is from occlum.

use core::sync::atomic::{Ordering, fence};

use ostd::{mm::VmIo, task::Task};

use crate::{
    context::current_userspace,
    prelude::*,
    process::posix_thread::futex::{
        FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, FutexVisibility, futex_wake,
    },
    thread::Tid,
};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct RobustList {
    next: Vaddr, // *const Robust list
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(crate) struct RobustListHead {
    /// Linked list of lock entries
    ///
    /// If it points to the head of the list, then it is the end of the list.
    /// If it is an invalid user space pointer or a null pointer, stop iterating
    /// the list.
    list: RobustList,
    /// Specifies the offset from the address of the lock entry to the address
    /// of the futex.
    futex_offset: isize,
    /// Contains transient copy of the address of the lock entry, during list
    /// insertion and removal.
    list_op_pending: Vaddr, // *const RobustList
}

impl RobustListHead {
    /// Return an iterator for all futexes in the robust list.
    ///
    /// The futex referred to by `list_op_pending`, if any, will be returned as
    /// the last item.
    pub(crate) fn futexes(&self, head_addr: Vaddr) -> FutexIter<'_> {
        FutexIter::new(self, head_addr)
    }

    /// Return the pending futex address if exist
    fn pending_futex_addr(&self) -> Option<RobustFutex> {
        if self.list_op_pending == 0 {
            None
        } else {
            self.futex_entry(self.list_op_pending)
        }
    }

    /// Get the futex address
    fn futex_addr(&self, entry_ptr: Vaddr) -> Option<Vaddr> {
        self.futex_offset
            .checked_add(entry_ptr as isize)
            .map(|result| result as Vaddr)
    }

    fn futex_entry(&self, tagged_entry: Vaddr) -> Option<RobustFutex> {
        let address = self.futex_addr(tagged_entry & !1)?;
        Some(if tagged_entry & 1 == 0 {
            RobustFutex::Ordinary(address)
        } else {
            RobustFutex::PriorityInheritance(address)
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RobustFutex {
    Ordinary(Vaddr),
    PriorityInheritance(Vaddr),
}

impl RobustFutex {
    pub(crate) fn address(self) -> Vaddr {
        match self {
            Self::Ordinary(address) | Self::PriorityInheritance(address) => address,
        }
    }
}

pub(crate) struct FutexIter<'a> {
    robust_list: &'a RobustListHead,
    entry_ptr: Vaddr,
    head_addr: Vaddr,
    count: isize,
}

impl<'a> FutexIter<'a> {
    pub(crate) fn new(robust_list: &'a RobustListHead, head_addr: Vaddr) -> Self {
        Self {
            robust_list,
            entry_ptr: robust_list.list.next,
            head_addr,
            count: 0,
        }
    }

    // The `self.count` is normally a positive value used to iterate the list
    // to avoid excessively long or circular list, we use a special value -1
    // to represent the end of the Iterator.
    fn set_end(&mut self) {
        self.count = -1;
    }

    fn is_end(&self) -> bool {
        self.count < 0
    }
}

const ROBUST_LIST_LIMIT: isize = 2048;

impl Iterator for FutexIter<'_> {
    type Item = RobustFutex;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_end() {
            return None;
        }

        while self.entry_ptr & !1 != self.head_addr {
            if self.count == ROBUST_LIST_LIMIT {
                break;
            }
            if self.entry_ptr & !1 == 0 {
                return None;
            }
            let futex_addr = if self.entry_ptr & !1 != (self.robust_list.list_op_pending & !1) {
                self.robust_list.futex_entry(self.entry_ptr)
            } else {
                None
            };
            let Ok(robust_list) = current_userspace!().read_val::<RobustList>(self.entry_ptr & !1)
            else {
                return None;
            };
            self.entry_ptr = robust_list.next;
            self.count += 1;
            if futex_addr.is_some() {
                return futex_addr;
            }
        }
        self.set_end();
        self.robust_list.pending_futex_addr()
    }
}

/// Attempts to wake a robust futex owned by the given thread.
///
/// If the futex at `futex_addr` is still owned by `tid`, it is marked with
/// `FUTEX_OWNER_DIED` and one waiter (if any) is woken.
/// If the futex is owned by another thread, the operation is canceled.
pub(crate) fn wake_robust_futex(futex: RobustFutex, tid: Tid) -> Result<()> {
    let futex_addr = futex.address();
    if !futex_addr.is_multiple_of(align_of::<u32>()) {
        return_errno_with_message!(
            Errno::EINVAL,
            "the futex word is not aligned on a four-byte boundary"
        );
    }

    let task = Task::current().unwrap();
    let user_space = CurrentUserSpace::new(task.as_thread_local().unwrap());

    // Instantiate a reader and a writer pointing at the same `futex_addr`, set up
    // for the same length: the length of an `u32`.
    let (reader, writer) = user_space.reader_writer(futex_addr, size_of::<u32>())?;

    let mut old_val: u32 = reader.atomic_load()?;
    fence(Ordering::Release);
    loop {
        // This futex may be held by another thread. If so, do nothing.
        if old_val & FUTEX_TID_MASK != tid {
            break;
        }

        let new_val = (old_val & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        match writer.atomic_compare_exchange(&reader, old_val, new_val)? {
            (cur_val, false) => old_val = cur_val, // Try again with `cur_val`.
            (_, true) => {
                // Wake up one waiter and break out from the loop.
                if new_val & FUTEX_WAITERS != 0 && matches!(futex, RobustFutex::Ordinary(_)) {
                    debug!("wake the robust futex at {:#x}", futex_addr);
                    futex_wake(futex_addr, 1, FutexVisibility::Shared)?;
                }
                break;
            }
        }
    }

    Ok(())
}
