// SPDX-License-Identifier: MPL-2.0

//! State shared by all references to a file description.

use core::sync::atomic::Ordering;

use super::{
    AccessMode, AtomicStatusFlags, FileLike, StatusFlags, file_handle::StatusFlagsUpdate,
    flock::FlockItem,
};
use crate::{
    events::{IoEvents, Observer},
    fs::vfs::{
        inode_ext::InodeExt,
        notify,
        path::Path,
        range_lock::{RangeLockItem, RangeLockOwner, RangeLockType},
    },
    prelude::*,
    process::{
        Pid, PidIdentity, PidNamespace, Process,
        signal::{PollAdaptor, constants::SIGIO},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileDescriptionId(usize);

/// Common fields for a file description.
///
/// This type is intended to collect state that belongs to the file description rather than to a
/// specific file descriptor.
pub(crate) struct FileCommon {
    path: Path,
    access_mode: AccessMode,
    status_flags: AtomicStatusFlags,
    owner: FileOwner,
    lock_identity: Arc<()>,
}

impl FileCommon {
    /// Creates common state for a file description.
    pub(crate) fn new(path: Path, access_mode: AccessMode, status_flags: StatusFlags) -> Self {
        Self {
            path,
            access_mode,
            status_flags: AtomicStatusFlags::new(status_flags),
            owner: FileOwner::new(),
            lock_identity: Arc::new(()),
        }
    }

    /// Returns the path associated with the file description.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the access mode of the file description.
    pub(crate) fn access_mode(&self) -> AccessMode {
        self.access_mode
    }

    /// Returns the current file status flags.
    pub(crate) fn status_flags(&self) -> StatusFlags {
        self.status_flags.load(Ordering::Relaxed)
    }

    /// Returns whether the file description is in non-blocking mode.
    pub(crate) fn is_nonblocking(&self) -> bool {
        self.status_flags().contains(StatusFlags::O_NONBLOCK)
    }

    /// Atomically updates the file status flags.
    pub(super) fn update_status_flags(&self, file: &dyn FileLike, update: StatusFlagsUpdate) {
        if !update.affects(StatusFlags::O_ASYNC) {
            self.apply_status_flags(update);
            return;
        }

        let mut owner_guard = self.owner.inner.lock();
        if let Some(owner) = owner_guard.as_mut() {
            if update.flags().contains(StatusFlags::O_ASYNC) {
                owner.register_observer(file);
            } else {
                owner.unregister_observer();
            }
        }
        self.apply_status_flags(update);
    }

    fn apply_status_flags(&self, update: StatusFlagsUpdate) {
        self.status_flags
            .update(Ordering::Relaxed, Ordering::Relaxed, |status_flags| {
                update.apply(status_flags)
            });
    }

    pub(crate) fn description_id(&self) -> FileDescriptionId {
        FileDescriptionId(Arc::as_ptr(&self.lock_identity) as usize)
    }

    pub(crate) fn test_range_lock(&self, mut lock: RangeLockItem) -> Result<RangeLockItem> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        let Some(range_lock_list) = self
            .path()
            .inode()
            .fs_lock_context()
            .map(|c| c.range_lock_list())
        else {
            // The lock list is not present. So nothing is locked.
            lock.set_type(RangeLockType::Unlock);
            return Ok(lock);
        };

        let req_lock = range_lock_list.test_lock(lock);
        Ok(req_lock)
    }

    pub(crate) fn set_range_lock(&self, lock: &RangeLockItem, is_nonblocking: bool) -> Result<()> {
        match lock.type_() {
            RangeLockType::ReadLock => {
                if self.status_flags().contains(StatusFlags::O_PATH)
                    || !self.access_mode().is_readable()
                {
                    return_errno_with_message!(Errno::EBADF, "the file is not opened readable");
                }
            }
            RangeLockType::WriteLock => {
                if self.status_flags().contains(StatusFlags::O_PATH)
                    || !self.access_mode().is_writable()
                {
                    return_errno_with_message!(Errno::EBADF, "the file is not opened writable");
                }
            }
            RangeLockType::Unlock => {
                if self.status_flags().contains(StatusFlags::O_PATH) {
                    return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
                }
            }
        }

        if RangeLockType::Unlock == lock.type_() {
            self.unlock_range_lock(lock);
            return Ok(());
        }

        let range_lock_list = self
            .path()
            .inode()
            .fs_lock_context_or_init()
            .range_lock_list();
        range_lock_list.set_lock(lock, is_nonblocking)
    }

    pub(crate) fn release_range_locks(&self, owner: RangeLockOwner) {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return;
        }

        if let Some(range_lock_list) = self
            .path()
            .inode()
            .fs_lock_context()
            .map(|context| context.range_lock_list())
        {
            range_lock_list.unlock_all(owner);
        }
    }

    fn unlock_range_lock(&self, lock: &RangeLockItem) {
        if let Some(range_lock_list) = self
            .path()
            .inode()
            .fs_lock_context()
            .map(|c| c.range_lock_list())
        {
            range_lock_list.unlock(lock);
        }
    }

    pub(crate) fn set_flock(&self, lock: FlockItem, is_nonblocking: bool) -> Result<()> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        let flock_list = self.path().inode().fs_lock_context_or_init().flock_list();
        flock_list.set_lock(lock, is_nonblocking)
    }

    pub(crate) fn unlock_flock(&self) -> Result<()> {
        if self.status_flags().contains(StatusFlags::O_PATH) {
            return_errno_with_message!(Errno::EBADF, "the file is opened as a path");
        }

        if let Some(flock_list) = self
            .path()
            .inode()
            .fs_lock_context()
            .map(|c| c.flock_list())
        {
            flock_list.unlock(self.description_id());
        }

        Ok(())
    }

    /// Returns the asynchronous I/O signal owner.
    pub(crate) fn owner(&self) -> &FileOwner {
        &self.owner
    }
}

impl Drop for FileCommon {
    fn drop(&mut self) {
        self.release_range_locks(RangeLockOwner::FileDescription(self.description_id()));
        let _ = self.unlock_flock();
        notify::on_close(self);
    }
}

/// The process that receives asynchronous I/O signals for a file description.
pub(crate) struct FileOwner {
    inner: Mutex<Option<Owner>>,
}

impl FileOwner {
    /// Creates an owner state with no process assigned.
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Returns the process ID of the current owner.
    pub(crate) fn pid_in(&self, namespace: &PidNamespace) -> Option<Pid> {
        self.inner
            .lock()
            .as_ref()
            .map(|owner| owner.identity.in_namespace(namespace))
    }

    pub(super) fn set(&self, file: &dyn FileLike, owner: Option<&Arc<Process>>) {
        let mut owner_guard = self.inner.lock();
        *owner_guard = None;

        let Some(process) = owner else {
            return;
        };

        let mut owner = Owner::new(process);
        if file.status_flags().contains(StatusFlags::O_ASYNC) {
            owner.register_observer(file);
        }
        *owner_guard = Some(owner);
    }
}

impl Default for FileOwner {
    fn default() -> Self {
        Self::new()
    }
}

struct Owner {
    identity: Arc<PidIdentity>,
    process: Weak<Process>,
    poller: Option<PollAdaptor<OwnerObserver>>,
}

impl Owner {
    fn new(process: &Arc<Process>) -> Self {
        Self {
            identity: process.pid_identity().clone(),
            process: Arc::downgrade(process),
            poller: None,
        }
    }

    fn register_observer(&mut self, file: &dyn FileLike) {
        if self.poller.is_some() {
            return;
        }

        let mut poller = PollAdaptor::with_observer(OwnerObserver::new(self.process.clone()));
        file.poll(IoEvents::IN | IoEvents::OUT, Some(poller.as_handle_mut()));
        self.poller = Some(poller);
    }

    fn unregister_observer(&mut self) {
        self.poller = None;
    }
}

struct OwnerObserver {
    owner: Weak<Process>,
}

impl OwnerObserver {
    fn new(owner: Weak<Process>) -> Self {
        Self { owner }
    }
}

impl Observer<IoEvents> for OwnerObserver {
    fn on_events(&self, _events: &IoEvents) {
        crate::process::enqueue_signal_async(self.owner.clone(), SIGIO);
    }
}
