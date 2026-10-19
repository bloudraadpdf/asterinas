// SPDX-License-Identifier: MPL-2.0

use aster_util::ranged_integer::RangedU32;

use super::{IPC_PRIVATE, IpcFlags, IpcKey};
use crate::prelude::*;

/// An IPC ID.
pub(crate) type IpcId = RangedU32<1, { i32::MAX as u32 }>;

pub(super) struct IpcIds<T> {
    objects: RwMutex<Objects<T>>,
    limit: usize,
}

struct Objects<T> {
    by_id: BTreeMap<IpcId, Entry<T>>,
    by_key: BTreeMap<IpcKey, IpcId>,
    next_id: u32,
}

struct Entry<T> {
    key: IpcKey,
    value: T,
}

pub(super) enum IpcRemoval {
    Destroy,
    Unname,
    Keep,
}

impl<T> IpcIds<T> {
    pub(super) fn new(limit: usize) -> Self {
        assert!(limit > 0 && limit <= i32::MAX as usize);
        Self {
            objects: RwMutex::new(Objects {
                by_id: BTreeMap::new(),
                by_key: BTreeMap::new(),
                next_id: 1,
            }),
            limit,
        }
    }

    pub(super) fn with<R>(
        &self,
        id: IpcId,
        op: impl FnOnce(&T) -> R,
    ) -> Result<R, IdNotExistError> {
        let objects = self.objects.read();
        let entry = objects.by_id.get(&id).ok_or(IdNotExistError)?;
        Ok(op(&entry.value))
    }

    pub(super) fn update<R>(
        &self,
        id: IpcId,
        op: impl FnOnce(&mut T) -> R,
    ) -> Result<R, IdNotExistError> {
        let mut objects = self.objects.write();
        let entry = objects.by_id.get_mut(&id).ok_or(IdNotExistError)?;
        Ok(op(&mut entry.value))
    }

    pub(super) fn remove(
        &self,
        id: IpcId,
        may_remove: impl FnOnce(&T) -> Result<IpcRemoval>,
    ) -> Result<()> {
        let mut objects = self.objects.write();
        let entry = objects.by_id.get(&id).ok_or(IdNotExistError)?;
        let key = entry.key;
        match may_remove(&entry.value)? {
            IpcRemoval::Destroy => {
                objects.by_id.remove(&id);
            }
            IpcRemoval::Unname => {
                objects.by_id.get_mut(&id).unwrap().key = IPC_PRIVATE;
            }
            IpcRemoval::Keep => return Ok(()),
        }
        if key != IPC_PRIVATE {
            objects.by_key.remove(&key);
        }
        Ok(())
    }

    pub(super) fn get_or_create(
        &self,
        key: IpcKey,
        flags: IpcFlags,
        validate_creation: Result<()>,
        create: impl FnOnce(IpcId) -> Result<T>,
        validate_existing: impl FnOnce(&T) -> Result<()>,
    ) -> Result<IpcId> {
        let mut objects = self.objects.write();
        if key != IPC_PRIVATE {
            if let Some(&id) = objects.by_key.get(&key) {
                if flags.contains(IpcFlags::IPC_CREAT | IpcFlags::IPC_EXCL) {
                    return_errno_with_message!(Errno::EEXIST, "the IPC key already exists");
                }
                validate_existing(&objects.by_id.get(&id).unwrap().value)?;
                return Ok(id);
            }
            if !flags.contains(IpcFlags::IPC_CREAT) {
                return_errno_with_message!(Errno::ENOENT, "the IPC key does not exist");
            }
        }
        validate_creation?;
        if objects.by_id.len() >= self.limit {
            return_errno_with_message!(Errno::ENOSPC, "the IPC namespace object limit is reached");
        }
        let id = loop {
            let id = IpcId::new(objects.next_id);
            objects.next_id = if objects.next_id == i32::MAX as u32 {
                1
            } else {
                objects.next_id + 1
            };
            if !objects.by_id.contains_key(&id) {
                break id;
            }
        };
        let value = create(id)?;
        objects.by_id.insert(id, Entry { key, value });
        if key != IPC_PRIVATE {
            objects.by_key.insert(key, id);
        }
        Ok(id)
    }
}

#[derive(Debug)]
pub(super) struct IdNotExistError;

impl From<IdNotExistError> for Error {
    fn from(_value: IdNotExistError) -> Self {
        Error::with_message(Errno::EINVAL, "the ID does not exist")
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    fn create(table: &IpcIds<u32>, key: IpcKey) -> Result<IpcId> {
        table.get_or_create(
            key,
            IpcFlags::IPC_CREAT,
            Ok(()),
            |id| Ok(id.get()),
            |_| Ok(()),
        )
    }

    #[ktest]
    fn keys_do_not_collide_and_released_ids_are_not_immediately_reused() {
        let table = IpcIds::new(2);
        let first = create(&table, 1).unwrap();
        let second = create(&table, 32001).unwrap();
        assert_ne!(first, second);
        assert_eq!(create(&table, 1).unwrap(), first);
        assert_eq!(create(&table, 2).unwrap_err().error(), Errno::ENOSPC);
        table.remove(first, |_| Ok(IpcRemoval::Destroy)).unwrap();
        let replacement = create(&table, 1).unwrap();
        assert_ne!(first, replacement);
        assert!(table.with(first, |_| ()).is_err());
        assert_eq!(table.with(second, |value| *value).unwrap(), second.get());
    }

    #[ktest]
    fn deferred_removal_releases_the_name_before_the_identifier() {
        let table = IpcIds::new(2);
        let first = create(&table, 15).unwrap();
        table.remove(first, |_| Ok(IpcRemoval::Unname)).unwrap();
        let second = create(&table, 15).unwrap();
        assert_ne!(first, second);
        assert_eq!(table.with(first, |value| *value).unwrap(), first.get());
        table.remove(first, |_| Ok(IpcRemoval::Destroy)).unwrap();
        assert_eq!(create(&table, 15).unwrap(), second);
    }

    #[ktest]
    fn private_keys_and_failed_constructors_do_not_publish_names_or_exhaust_slots() {
        let table = IpcIds::new(2);
        let first = create(&table, IPC_PRIVATE).unwrap();
        let failed = table.get_or_create(
            10,
            IpcFlags::IPC_CREAT,
            Ok(()),
            |_| Err(Error::new(Errno::ENOMEM)),
            |_| Ok(()),
        );
        assert_eq!(failed.unwrap_err().error(), Errno::ENOMEM);
        let second = create(&table, IPC_PRIVATE).unwrap();
        assert_ne!(first, second);
        assert_eq!(create(&table, 10).unwrap_err().error(), Errno::ENOSPC);
    }
}
