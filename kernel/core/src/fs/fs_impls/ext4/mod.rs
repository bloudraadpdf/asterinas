// SPDX-License-Identifier: MPL-2.0

//! Ext4 mounts with exclusive journal ownership and demand-paged file data.

use aster_block::BlockDevice;
use ostd::sync::WaitQueue;
use spin::Once;

use crate::{
    fs::{
        file::SyncMode,
        utils::{NAME_MAX, block_io::BlockIo},
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, FsFlags, SuperBlock},
            inode::Inode,
            registry::FsCreationCtx,
        },
    },
    prelude::*,
};

mod inode;
#[cfg(ktest)]
mod tests;
mod volume;

use inode::Ext4Inode;
use volume::Volume;

use super::disk::DiskFsType;

pub(in crate::fs) static EXT4_TYPE: DiskFsType = DiskFsType::new(
    "ext4",
    FsFlags::RDONLY
        .union(FsFlags::DIRSYNC)
        .union(FsFlags::SILENT),
    create,
);

pub(super) fn init() {
    crate::fs::vfs::registry::register(&EXT4_TYPE).unwrap();
}

fn create(ctx: &mut FsCreationCtx) -> Result<Arc<dyn FileSystem>> {
    let flags = ctx.flags();
    let disk = ctx.resolve_block_device()?.clone();
    let device = BlockIo(disk.clone());
    device.check_magic(1024 + 0x38, 0xef53u16.to_le_bytes())?;
    let volume = Volume::open(device, flags)?;
    let fs = Arc::new(Ext4 {
        volume: Arc::new(Mutex::new(volume)),
        disk,
        flags,
        root: Once::new(),
        inodes: Mutex::new(BTreeMap::new()),
        changed: WaitQueue::new(),
        stats: FsEventSubscriberStats::new(),
    });
    let root = fs.inode(2)?;
    fs.root.call_once(|| root);
    Ok(fs)
}

enum InodeEntry {
    Creating,
    Ready(Weak<Ext4Inode>),
}

struct Ext4 {
    volume: Arc<Mutex<Volume>>,
    disk: Arc<dyn BlockDevice>,
    flags: FsFlags,
    root: Once<Arc<Ext4Inode>>,
    inodes: Mutex<BTreeMap<u32, InodeEntry>>,
    changed: WaitQueue,
    stats: FsEventSubscriberStats,
}

impl Ext4 {
    fn inode(self: &Arc<Self>, number: u32) -> Result<Arc<Ext4Inode>> {
        let existing = self.changed.wait_until(|| {
            let mut inodes = self.inodes.lock();
            match inodes.get(&number) {
                Some(InodeEntry::Ready(weak)) => weak.upgrade().map(Some),
                Some(InodeEntry::Creating) => None,
                None => {
                    inodes.insert(number, InodeEntry::Creating);
                    Some(None)
                }
            }
        });
        if let Some(inode) = existing {
            return Ok(inode);
        }
        let result = Ext4Inode::new(self, number);
        {
            let mut inodes = self.inodes.lock();
            match &result {
                Ok(inode) => {
                    inodes.insert(number, InodeEntry::Ready(Arc::downgrade(inode)));
                }
                Err(_) => {
                    inodes.remove(&number);
                }
            }
        }
        self.changed.wake_all();
        result
    }

    fn forget(&self, inode: &Ext4Inode) {
        let mut inodes = self.inodes.lock();
        if matches!(inodes.get(&inode.number), Some(InodeEntry::Ready(weak)) if core::ptr::eq(weak.as_ptr(), inode))
        {
            inodes.remove(&inode.number);
        }
        drop(inodes);
        self.changed.wake_all();
    }
}

impl FileSystem for Ext4 {
    fn name(&self) -> &'static str {
        "ext4"
    }
    fn sync(&self) -> Result<()> {
        let mut after = None;
        loop {
            let next = self.changed.wait_until(|| {
                let inodes = self.inodes.lock();
                let start = after.map_or(core::ops::Bound::Unbounded, core::ops::Bound::Excluded);
                match inodes.range((start, core::ops::Bound::Unbounded)).next() {
                    Some((&number, InodeEntry::Ready(weak))) => {
                        weak.upgrade().map(|inode| Some((number, inode)))
                    }
                    Some((_, InodeEntry::Creating)) => None,
                    None => Some(None),
                }
            });
            let Some((number, inode)) = next else {
                break;
            };
            inode.sync(SyncMode::Full)?;
            after = Some(number);
        }
        self.volume.lock().sync()
    }
    fn root_inode(&self) -> Arc<dyn Inode> {
        self.root.get().unwrap().clone()
    }
    fn sb(&self) -> SuperBlock {
        let volume = self.volume.lock();
        let geometry = volume.geometry();
        let mut sb = SuperBlock::new(
            0xef53,
            geometry.block_size() as usize,
            NAME_MAX,
            self.disk.id(),
        );
        sb.blocks = geometry.blocks() as usize;
        sb.bfree = geometry.free_blocks() as usize;
        sb.bavail = geometry.available_blocks() as usize;
        sb.files = geometry.inodes() as usize;
        sb.ffree = geometry.free_inodes() as usize;
        sb.flags = self.flags.bits() as u64;
        sb
    }
    fn flags(&self) -> FsFlags {
        self.flags
    }
    fn set_fs_flags(&self, flags: FsFlags, data: Option<&str>, _ctx: &Context) -> Result<()> {
        if flags != self.flags || data.is_some_and(|data| !data.is_empty()) {
            return_errno!(Errno::EOPNOTSUPP);
        }
        Ok(())
    }
    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.stats
    }
}

fn map_error(error: aster_ext4::Error) -> Error {
    use aster_ext4::Error as E;
    Error::new(match error {
        E::Unsupported => Errno::EOPNOTSUPP,
        E::NeedsRecovery | E::Corrupt | E::Aborted | E::Io(_) => Errno::EIO,
        E::ReadOnly => Errno::EROFS,
        E::ResourceLimit => Errno::E2BIG,
        E::InvalidInput => Errno::EINVAL,
        E::NotDirectory => Errno::ENOTDIR,
        E::IsDirectory => Errno::EISDIR,
        E::NotEmpty => Errno::ENOTEMPTY,
        E::NoAttribute => Errno::ENODATA,
        E::NotFound => Errno::ENOENT,
        E::NameTooLong => Errno::ENAMETOOLONG,
        E::NoSpace => Errno::ENOSPC,
        E::AlreadyExists => Errno::EEXIST,
        E::PermissionDenied => Errno::EPERM,
        E::TooManyLinks => Errno::EMLINK,
    })
}
