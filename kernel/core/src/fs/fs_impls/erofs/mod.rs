// SPDX-License-Identifier: MPL-2.0

//! EROFS mounts and demand-paged immutable inodes.

use core::time::Duration;

use aster_block::BlockDevice;
use device_id::DeviceId;
use io_util::batch::IoBatch;
use ostd::mm::VmIo;
use spin::Once;

use super::disk::DiskFsType;
use crate::{
    fs::{
        file::{AccessMode, InodeMode, InodeType, PerOpenFileOps, StatusFlags},
        pipe::Pipe,
        utils::{
            DirentVisitor, NAME_MAX, PATH_MAX,
            block_io::{BlockIo, read_cache_page},
        },
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, FsFlags, SuperBlock},
            inode::{Extension, FileOps, Inode, Metadata, SymbolicLink},
            path::Dentry,
            registry::FsCreationCtx,
            xattr::{XATTR_LIST_MAX_LEN, XattrName, XattrNamespace},
        },
    },
    prelude::*,
    process::{Gid, Uid},
    vm::page_cache::{LockedCachePage, PageCache, PageCacheBackend, Vmo},
};

pub(in crate::fs) static EROFS_TYPE: DiskFsType =
    DiskFsType::new("erofs", FsFlags::RDONLY.union(FsFlags::SILENT), create);

pub(super) fn init() {
    crate::fs::vfs::registry::register(&EROFS_TYPE).unwrap();
}

fn create(ctx: &mut FsCreationCtx) -> Result<Arc<dyn FileSystem>> {
    let flags = ctx.flags() | FsFlags::RDONLY;
    let disk = ctx.resolve_block_device()?.clone();
    let device = BlockIo(disk.clone());
    device.check_magic(1024, 0xe0f5_e1e2u32.to_le_bytes())?;
    let volume = aster_erofs::Erofs::open(device).map_err(map_error)?;
    let root = volume.inode(volume.root_nid()).map_err(map_error)?;
    let volume = Arc::new(Mutex::new(volume));
    let fs = Arc::new(Erofs {
        flags,
        volume,
        disk,
        root: Once::new(),
        inodes: Mutex::new(BTreeMap::new()),
        stats: FsEventSubscriberStats::new(),
    });
    let root = fs.inode(root)?;
    fs.root.call_once(|| root);
    Ok(fs)
}

struct Erofs {
    flags: FsFlags,
    volume: Arc<Mutex<aster_erofs::Erofs<BlockIo>>>,
    disk: Arc<dyn BlockDevice>,
    root: Once<Arc<ErofsInode>>,
    inodes: Mutex<BTreeMap<u64, Weak<ErofsInode>>>,
    stats: FsEventSubscriberStats,
}

impl Erofs {
    fn inode(self: &Arc<Self>, raw: aster_erofs::Inode) -> Result<Arc<ErofsInode>> {
        let mut inodes = self.inodes.lock();
        if let Some(inode) = inodes.get(&raw.nid).and_then(Weak::upgrade) {
            return Ok(inode);
        }
        let type_ = InodeType::from_raw_mode(raw.mode)?;
        let backend = Arc::new(DataBackend {
            volume: self.volume.clone(),
            raw: raw.clone(),
        });
        let page_cache =
            PageCache::new_with_backend(raw.size as usize, Arc::downgrade(&backend) as _)?;
        let inode = Arc::new(ErofsInode {
            fs: Arc::downgrade(self),
            raw,
            type_,
            page_cache,
            backend,
            pipe: (type_ == InodeType::NamedPipe).then(Pipe::new),
            extension: Extension::new(),
        });
        inodes.insert(inode.raw.nid, Arc::downgrade(&inode));
        Ok(inode)
    }
}

impl FileSystem for Erofs {
    fn name(&self) -> &'static str {
        "erofs"
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    fn root_inode(&self) -> Arc<dyn Inode> {
        self.root.get().unwrap().clone()
    }
    fn flags(&self) -> FsFlags {
        self.flags
    }
    fn set_fs_flags(&self, flags: FsFlags, _data: Option<&str>, _ctx: &Context) -> Result<()> {
        if !flags.contains(FsFlags::RDONLY) {
            return_errno!(Errno::EROFS);
        }
        if flags != self.flags {
            return_errno!(Errno::EOPNOTSUPP);
        }
        Ok(())
    }
    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.stats
    }
    fn sb(&self) -> SuperBlock {
        let volume = self.volume.lock();
        let mut sb = SuperBlock::new(
            0xe0f5_e1e2,
            volume.block_size() as usize,
            NAME_MAX,
            self.disk.id(),
        );
        sb.blocks = (volume.image_size() / volume.block_size()) as usize;
        sb.files = volume.inode_count() as usize;
        sb.flags = self.flags.bits() as u64;
        sb
    }
}

struct ErofsInode {
    fs: Weak<Erofs>,
    raw: aster_erofs::Inode,
    type_: InodeType,
    page_cache: PageCache,
    backend: Arc<DataBackend>,
    pipe: Option<Pipe>,
    extension: Extension,
}

impl Drop for ErofsInode {
    fn drop(&mut self) {
        if let Some(fs) = self.fs.upgrade() {
            let mut inodes = fs.inodes.lock();
            if inodes
                .get(&self.raw.nid)
                .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), self))
            {
                inodes.remove(&self.raw.nid);
            }
        }
    }
}

impl FileOps for ErofsInode {
    fn read_at(&self, offset: usize, writer: &mut VmWriter, _flags: StatusFlags) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        let count = writer.avail().min(self.size().saturating_sub(offset));
        if count == 0 {
            return Ok(0);
        }
        writer.limit(count);
        self.page_cache.read(offset, writer)?;
        Ok(count)
    }
    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _flags: StatusFlags,
    ) -> Result<usize> {
        return_errno!(Errno::EROFS);
    }
    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        let fs = self.fs.upgrade().unwrap();
        let mut position = offset as u64;
        loop {
            let entry = fs
                .volume
                .lock()
                .next_entry(&self.raw, position)
                .map_err(map_error)?;
            let Some(entry) = entry else {
                break;
            };
            let name = core::str::from_utf8(&entry.name).map_err(|_| Error::new(Errno::EIO))?;
            let raw = fs.volume.lock().inode(entry.nid).map_err(map_error)?;
            let type_ = InodeType::from_raw_mode(raw.mode)?;
            if let Err(error) = visitor.visit(name, entry.nid, type_, entry.next_offset as usize) {
                if position == offset as u64 {
                    return Err(error);
                }
                break;
            }
            position = entry.next_offset;
        }
        Ok(position as usize - offset)
    }
}

impl Inode for ErofsInode {
    fn size(&self) -> usize {
        self.raw.size as usize
    }
    fn resize(&self, _dentry: &Dentry, _size: usize) -> Result<()> {
        return_errno!(Errno::EROFS);
    }
    fn ino(&self) -> u64 {
        self.raw.nid
    }
    fn type_(&self) -> InodeType {
        self.type_
    }
    fn mode(&self) -> Result<InodeMode> {
        Ok(InodeMode::from_bits_truncate(self.raw.mode))
    }
    fn set_mode(&self, _dentry: &Dentry, _mode: InodeMode) -> Result<()> {
        return_errno!(Errno::EROFS);
    }
    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new(self.raw.uid))
    }
    fn set_owner(&self, _dentry: &Dentry, _uid: Uid) -> Result<()> {
        return_errno!(Errno::EROFS);
    }
    fn group(&self) -> Result<Gid> {
        Ok(Gid::new(self.raw.gid))
    }
    fn set_group(&self, _dentry: &Dentry, _gid: Gid) -> Result<()> {
        return_errno!(Errno::EROFS);
    }
    fn atime(&self) -> Duration {
        self.mtime()
    }
    fn set_atime(&self, _dentry: &Dentry, _time: Duration) {}
    fn mtime(&self) -> Duration {
        Duration::new(self.raw.mtime_seconds, self.raw.mtime_nanoseconds)
    }
    fn set_mtime(&self, _dentry: &Dentry, _time: Duration) {}
    #[cfg(any(feature = "filesystems_extra", ktest))]
    fn ctime(&self) -> Duration {
        self.mtime()
    }
    fn set_ctime(&self, _dentry: &Dentry, _time: Duration) {}
    fn page_cache(&self) -> Option<Arc<Vmo>> {
        (self.type_ == InodeType::File).then(|| self.page_cache.as_vmo().clone())
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }
    fn extension(&self) -> &Extension {
        &self.extension
    }
    fn metadata(&self) -> Result<Metadata> {
        let fs = self.fs.upgrade().unwrap();
        let (block_size, sectors) = {
            let volume = self.backend.volume.lock();
            let sectors = usize::try_from(volume.allocated_sectors(&self.raw))
                .map_err(|_| Error::new(Errno::EOVERFLOW))?;
            (volume.block_size() as usize, sectors)
        };
        let self_dev_id = self
            .raw
            .device_number()
            .map(|(major, minor)| {
                DeviceId::from_encoded_u64(device_id::encode_device_numbers(major, minor))
                    .ok_or_else(|| Error::new(Errno::EIO))
            })
            .transpose()?;
        Ok(Metadata {
            ino: self.raw.nid,
            size: self.size(),
            optimal_block_size: block_size,
            nr_sectors_allocated: sectors,
            last_access_at: self.mtime(),
            last_modify_at: self.mtime(),
            last_meta_change_at: self.mtime(),
            type_: self.type_,
            mode: self.mode()?,
            nr_hard_links: self.raw.links as usize,
            uid: self.owner()?,
            gid: self.group()?,
            container_dev_id: fs.disk.id(),
            self_dev_id,
            birth_at: None,
        })
    }
    fn open(
        &self,
        _dentry: &Dentry,
        access: AccessMode,
        flags: StatusFlags,
    ) -> Option<Result<Box<dyn PerOpenFileOps>>> {
        crate::fs::utils::device_inode::open(self, self.pipe.as_ref(), access, flags)
    }
    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let fs = self.fs.upgrade().unwrap();
        let raw = fs
            .volume
            .lock()
            .lookup(&self.raw, name.as_bytes())
            .map_err(map_error)?;
        Ok(fs.inode(raw)?)
    }
    fn get_xattr(&self, name: XattrName, writer: &mut VmWriter) -> Result<usize> {
        let volume = &self.backend.volume;
        let count = volume
            .lock()
            .get_xattr(&self.raw, name.full_name().as_bytes(), &mut [])
            .map_err(map_xattr_error)?;
        if writer.avail() == 0 {
            return Ok(count);
        }
        if writer.avail() < count {
            return_errno!(Errno::ERANGE);
        }
        let mut bytes = vec![0; count];
        volume
            .lock()
            .get_xattr(&self.raw, name.full_name().as_bytes(), &mut bytes)
            .map_err(map_xattr_error)?;
        writer.write_fallible(&mut VmReader::from(bytes.as_slice()))?;
        Ok(count)
    }

    fn list_xattr(&self, namespace: XattrNamespace, writer: &mut VmWriter) -> Result<usize> {
        let mut names = Vec::new();
        self.backend
            .volume
            .lock()
            .list_xattrs(&self.raw, |bytes| {
                let name = core::str::from_utf8(bytes).map_err(|_| aster_erofs::Error::Corrupt)?;
                if namespace != XattrNamespace::User
                    || XattrNamespace::try_from_full_name(name) == Some(XattrNamespace::User)
                {
                    if names.len() + bytes.len() + 1 > XATTR_LIST_MAX_LEN {
                        return Err(aster_erofs::Error::BufferTooSmall);
                    }
                    names.extend_from_slice(bytes);
                    names.push(0);
                }
                Ok(())
            })
            .map_err(map_error)?;
        if writer.avail() == 0 {
            return Ok(names.len());
        }
        if writer.avail() < names.len() {
            return_errno!(Errno::ERANGE);
        }
        writer.write_fallible(&mut VmReader::from(names.as_slice()))?;
        Ok(names.len())
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }
        if self.size() > PATH_MAX {
            return_errno!(Errno::ENAMETOOLONG);
        }
        let fs = self.fs.upgrade().unwrap();
        let mut bytes = vec![0; self.size()];
        fs.volume
            .lock()
            .read_at(&self.raw, 0, &mut bytes)
            .map_err(map_error)?;
        let target = String::from_utf8(bytes).map_err(|_| Error::new(Errno::EIO))?;
        Ok(SymbolicLink::Plain(target))
    }
}

struct DataBackend {
    volume: Arc<Mutex<aster_erofs::Erofs<BlockIo>>>,
    raw: aster_erofs::Inode,
}

impl PageCacheBackend for DataBackend {
    fn read_page_async(
        &self,
        idx: usize,
        page: LockedCachePage,
        _batch: &mut IoBatch,
    ) -> Result<()> {
        read_cache_page(idx, page, |offset, bytes| {
            if offset as u64 >= self.raw.size {
                return_errno!(Errno::EINVAL);
            }
            self.volume
                .lock()
                .read_at(&self.raw, offset as u64, bytes)
                .map_err(map_error)?;
            Ok(())
        })
    }
    #[cfg(any(feature = "filesystems_extra", feature = "fs_ext4", ktest))]
    fn write_page_async(
        &self,
        _idx: usize,
        page: LockedCachePage,
        _batch: &mut IoBatch,
    ) -> Result<()> {
        page.set_dirty();
        return_errno!(Errno::EROFS);
    }
}

fn map_error(error: aster_erofs::Error) -> Error {
    Error::new(match error {
        aster_erofs::Error::Unsupported => Errno::EOPNOTSUPP,
        aster_erofs::Error::NotFound => Errno::ENOENT,
        aster_erofs::Error::BufferTooSmall => Errno::ERANGE,
        aster_erofs::Error::Corrupt | aster_erofs::Error::Io(_) => Errno::EIO,
    })
}

fn map_xattr_error(error: aster_erofs::Error) -> Error {
    if error == aster_erofs::Error::NotFound {
        Error::new(Errno::ENODATA)
    } else {
        map_error(error)
    }
}
