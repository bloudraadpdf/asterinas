// SPDX-License-Identifier: MPL-2.0

//! VFS inode identity, cached data and namespace operations.

use core::time::Duration;

use aster_ext4::{CreateMetadata, MetadataUpdate, SpecialFile};
use io_util::batch::IoBatch;
use ostd::mm::VmIo;

use super::{Ext4, Volume, map_error, volume::timestamp};
use crate::{
    fs::{
        file::{AccessMode, InodeMode, InodeType, PerOpenFileOps, StatusFlags, SyncMode},
        pipe::Pipe,
        utils::{DirentVisitor, PATH_MAX, block_io::read_cache_page, current_fs_ids},
        vfs::{
            file_system::FileSystem,
            inode::{
                Extension, FileOps, HardLinkability, Inode, Metadata, MknodType, ReaddirContext,
                RenameMode, SymbolicLink,
            },
            path::Dentry,
            xattr::{XattrName, XattrNamespace, XattrSetFlags},
        },
    },
    prelude::*,
    process::{Gid, Uid},
    time::clocks::RealTimeCoarseClock,
    vm::page_cache::{LockedCachePage, PageCache, PageCacheBackend, Vmo},
};

mod direct;
mod xattr;

enum Removal {
    File,
    Directory,
}

pub(super) struct Ext4Inode {
    fs: Weak<Ext4>,
    pub(super) number: u32,
    type_: InodeType,
    cache: Mutex<PageCache>,
    backend: Arc<DataBackend>,
    extension: Extension,
    pipe: Option<Pipe>,
    linkable: core::sync::atomic::AtomicBool,
}

impl Ext4Inode {
    pub(super) fn new(fs: &Arc<Ext4>, number: u32) -> Result<Arc<Self>> {
        let metadata = fs.volume.lock().load(number)?;
        let backend = Arc::new(DataBackend {
            volume: fs.volume.clone(),
            number,
        });
        let cache = match PageCache::new_with_backend(metadata.size, Arc::downgrade(&backend) as _)
        {
            Ok(cache) => cache,
            Err(error) => {
                fs.volume.lock().release(number)?;
                return Err(error);
            }
        };
        Ok(Arc::new(Self {
            fs: Arc::downgrade(fs),
            number,
            type_: metadata.type_,
            cache: Mutex::new(cache),
            backend,
            extension: Extension::new(),
            pipe: (metadata.type_ == InodeType::NamedPipe).then(Pipe::new),
            linkable: core::sync::atomic::AtomicBool::new(true),
        }))
    }

    fn check_regular_io(&self) -> Result<()> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        if self.type_ != InodeType::File {
            return_errno!(Errno::EINVAL);
        }
        Ok(())
    }

    fn volume(&self) -> &Mutex<Volume> {
        &self.backend.volume
    }
    fn cached_metadata(&self) -> Metadata {
        self.volume().lock().metadata(self.number)
    }
    fn now() -> Duration {
        RealTimeCoarseClock::get().read_time()
    }
    fn creation(mode: InodeMode) -> Result<CreateMetadata> {
        let (uid, gid) = current_fs_ids();
        let mut generation = [0; 4];
        crate::util::random::getrandom(&mut generation);
        Ok(CreateMetadata {
            permissions: mode.bits(),
            uid: uid.into(),
            gid: gid.into(),
            created: timestamp(Self::now())?,
            generation: u32::from_ne_bytes(generation),
        })
    }

    fn set_metadata(&self, update: MetadataUpdate) -> Result<()> {
        self.volume().lock().mutate(&[self.number], |writer| {
            writer.set_metadata(self.number, update)
        })
    }

    fn resize_cached(&self, cache: &mut PageCache, size: usize) -> Result<()> {
        if self.type_ != InodeType::File {
            return_errno!(Errno::EINVAL);
        }
        if size > isize::MAX as usize {
            return_errno!(Errno::EFBIG);
        }
        {
            let mut volume = self.volume().lock();
            volume.writer()?;
            if volume.flags(self.number) & 0x30 != 0 {
                return_errno!(Errno::EPERM);
            }
        }
        let old_size = self.size();
        if old_size == size {
            return Ok(());
        }
        if size < old_size {
            cache.flush_range(0..old_size)?;
        }
        let changed = timestamp(Self::now())?;
        if size < old_size {
            cache.resize(size, old_size)?;
        }
        let result = self.volume().lock().mutate(&[self.number], |writer| {
            writer.truncate(self.number, size as u64, changed)
        });
        if let Err(error) = result {
            if size < old_size {
                self.volume().lock().retain_error(&error);
            }
            return Err(error);
        }
        if size > old_size
            && let Err(error) = cache.resize(size, old_size)
        {
            self.volume().lock().retain_error(&error);
            return Err(error);
        }
        Ok(())
    }

    fn remove_child(&self, child: &Dentry, kind: Removal) -> Result<()> {
        let child_inode = self.same_fs(child.inode().as_ref())?;
        let name = child.name();
        let changed = timestamp(Self::now())?;
        self.volume()
            .lock()
            .mutate(&[self.number, child_inode.number], |writer| {
                if matches!(kind, Removal::Directory) {
                    writer.remove_directory(self.number, name.as_bytes(), changed)
                } else {
                    writer.unlink(self.number, name.as_bytes(), changed)
                }
            })?;
        Ok(())
    }

    fn same_fs<'a>(&self, other: &'a dyn Inode) -> Result<&'a Self> {
        let other = other
            .downcast_ref::<Self>()
            .ok_or_else(|| Error::new(Errno::EXDEV))?;
        if !Arc::ptr_eq(&self.backend.volume, &other.backend.volume) {
            return_errno!(Errno::EXDEV);
        }
        Ok(other)
    }

    fn create_with(
        &self,
        action: impl FnOnce(
            &mut aster_ext4::WritableExt4<crate::fs::utils::block_io::BlockIo>,
        ) -> core::result::Result<aster_ext4::Inode, aster_ext4::Error>,
    ) -> Result<Arc<dyn Inode>> {
        let raw = self.volume().lock().mutate(&[self.number], action)?;
        Ok(self.fs.upgrade().unwrap().inode(raw.number())?)
    }
}

impl Drop for Ext4Inode {
    fn drop(&mut self) {
        let result = if self.cached_metadata().nr_hard_links == 0 {
            self.volume().lock().release(self.number)
        } else {
            self.cache
                .lock()
                .flush_range(0..self.size())
                .and_then(|()| self.volume().lock().release(self.number))
        };
        if let Err(error) = result {
            self.volume().lock().retain_error(&error);
            error!("ext4 inode {} release failed: {:?}", self.number, error);
        }
        if let Some(fs) = self.fs.upgrade() {
            fs.forget(self);
        }
    }
}

impl FileOps for Ext4Inode {
    fn read_at(&self, offset: usize, writer: &mut VmWriter, flags: StatusFlags) -> Result<usize> {
        self.check_regular_io()?;
        if flags.contains(StatusFlags::O_DIRECT) {
            return self.read_direct_at(offset, writer);
        }
        let cache = self.cache.lock();
        self.volume().lock().reader()?;
        let count = writer.avail().min(self.size().saturating_sub(offset));
        if count == 0 {
            return Ok(0);
        }
        writer.limit(count);
        let before = writer.avail();
        let result = cache.read(offset, writer);
        let read = before - writer.avail();
        match result {
            Ok(()) => Ok(read),
            Err(_) if read != 0 => Ok(read),
            Err(error) => Err(error.into()),
        }
    }

    fn write_at(&self, offset: usize, reader: &mut VmReader, flags: StatusFlags) -> Result<usize> {
        self.check_regular_io()?;
        if flags.contains(StatusFlags::O_DIRECT) {
            return self.write_direct_at(offset, reader);
        }
        let mut cache = self.cache.lock();
        {
            let mut volume = self.volume().lock();
            volume.writer()?;
            let flags = volume.flags(self.number);
            if flags & 0x10 != 0 {
                return_errno!(Errno::EPERM);
            }
            if flags & 0x20 != 0 {
                return_errno!(Errno::EOPNOTSUPP);
            }
        }
        offset
            .checked_add(reader.remain())
            .filter(|&end| end <= isize::MAX as usize)
            .ok_or_else(|| Error::new(Errno::EFBIG))?;
        let mut written = 0;
        let mut bytes = vec![0; PAGE_SIZE];
        let result: Result<()> = (|| {
            while reader.remain() != 0 {
                let position = offset + written;
                let count = reader.remain().min(PAGE_SIZE - position % PAGE_SIZE);
                let (copied, fault) =
                    match VmWriter::from(&mut bytes[..count]).write_fallible(reader) {
                        Ok(count) => (count, None),
                        Err((error, count)) => (count, Some(error)),
                    };
                if copied != 0 {
                    let size = position + copied;
                    if size > self.size() {
                        self.resize_cached(&mut cache, size)?;
                    }
                    let mut staged = VmReader::from(&bytes[..copied]).to_fallible();
                    let result = cache.write(position, &mut staged);
                    written += copied - staged.remain();
                    result?;
                }
                if let Some(error) = fault {
                    return Err(error.into());
                }
            }
            Ok(())
        })();
        if written != 0 {
            let now = Self::now();
            self.volume().lock().change_times(self.number, |metadata| {
                metadata.last_modify_at = now;
                metadata.last_meta_change_at = now;
            });
            return Ok(written);
        }
        result.map(|()| 0)
    }

    fn readdir_at(
        &self,
        _context: &ReaddirContext<'_>,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        let mut position = offset;
        loop {
            let entries = {
                let volume = self.volume().lock();
                let reader = volume.reader()?;
                let raw = reader.inode(self.number).map_err(map_error)?;
                let mut entries = Vec::new();
                for entry in reader
                    .read_dir_from(&raw, position as u64)
                    .map_err(map_error)?
                    .take(32)
                {
                    let entry = entry.map_err(map_error)?;
                    let type_ = match entry.file_type() {
                        0 => InodeType::from_raw_mode(
                            reader.inode(entry.inode()).map_err(map_error)?.mode(),
                        )?,
                        1 => InodeType::File,
                        2 => InodeType::Dir,
                        3 => InodeType::CharDevice,
                        4 => InodeType::BlockDevice,
                        5 => InodeType::NamedPipe,
                        6 => InodeType::Socket,
                        7 => InodeType::SymLink,
                        _ => return_errno!(Errno::EIO),
                    };
                    entries.push((entry, type_));
                }
                entries
            };
            if entries.is_empty() {
                break;
            }
            for (entry, type_) in entries {
                let name =
                    core::str::from_utf8(entry.name()).map_err(|_| Error::new(Errno::EIO))?;
                let next = usize::try_from(entry.next_offset())
                    .map_err(|_| Error::new(Errno::EOVERFLOW))?;
                if let Err(error) = visitor.visit(name, u64::from(entry.inode()), type_, next) {
                    return if position == offset {
                        Err(error)
                    } else {
                        Ok(position - offset)
                    };
                }
                position = next;
            }
        }
        Ok(position - offset)
    }
}

impl Inode for Ext4Inode {
    fn size(&self) -> usize {
        self.cached_metadata().size
    }
    fn resize(&self, _dentry: &Dentry, size: usize) -> Result<()> {
        self.resize_cached(&mut self.cache.lock(), size)
    }
    fn metadata(&self) -> Result<Metadata> {
        let volume = self.volume().lock();
        volume.reader()?;
        Ok(volume.metadata(self.number))
    }
    fn ino(&self) -> u64 {
        u64::from(self.number)
    }
    fn type_(&self) -> InodeType {
        self.type_
    }
    fn mode(&self) -> Result<InodeMode> {
        Ok(self.cached_metadata().mode)
    }
    fn set_mode(&self, _dentry: &Dentry, mode: InodeMode) -> Result<()> {
        let mut update = MetadataUpdate::new(timestamp(Self::now())?);
        update.permissions = Some(mode.bits());
        self.set_metadata(update)
    }
    fn owner(&self) -> Result<Uid> {
        Ok(self.cached_metadata().uid)
    }
    fn set_owner(&self, _dentry: &Dentry, uid: Uid) -> Result<()> {
        let mut update = MetadataUpdate::new(timestamp(Self::now())?);
        update.uid = Some(uid.into());
        self.set_metadata(update)
    }
    fn group(&self) -> Result<Gid> {
        Ok(self.cached_metadata().gid)
    }
    fn set_group(&self, _dentry: &Dentry, gid: Gid) -> Result<()> {
        let mut update = MetadataUpdate::new(timestamp(Self::now())?);
        update.gid = Some(gid.into());
        self.set_metadata(update)
    }
    fn atime(&self) -> Duration {
        self.cached_metadata().last_access_at
    }
    fn set_atime(&self, _dentry: &Dentry, time: Duration) {
        self.volume()
            .lock()
            .change_times(self.number, |metadata| metadata.last_access_at = time);
    }
    fn mtime(&self) -> Duration {
        self.cached_metadata().last_modify_at
    }
    fn set_mtime(&self, _dentry: &Dentry, time: Duration) {
        self.volume()
            .lock()
            .change_times(self.number, |metadata| metadata.last_modify_at = time);
    }
    #[cfg(any(feature = "filesystems_extra", ktest))]
    fn ctime(&self) -> Duration {
        self.cached_metadata().last_meta_change_at
    }
    fn set_ctime(&self, _dentry: &Dentry, time: Duration) {
        self.volume()
            .lock()
            .change_times(self.number, |metadata| metadata.last_meta_change_at = time);
    }
    fn page_cache(&self) -> Option<Arc<Vmo>> {
        (self.type_ == InodeType::File && self.volume().lock().flags(self.number) & 0x30 == 0)
            .then(|| self.cache.lock().as_vmo().clone())
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }
    fn extension(&self) -> &Extension {
        &self.extension
    }
    fn sync(&self, _mode: SyncMode) -> Result<()> {
        let cache = self.cache.lock();
        cache.flush_range(0..self.size())?;
        let mut volume = self.volume().lock();
        volume.flush_times(self.number)?;
        volume.sync()
    }
    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let number = {
            let volume = self.volume().lock();
            let reader = volume.reader()?;
            let raw = reader.inode(self.number).map_err(map_error)?;
            reader
                .lookup(&raw, name.as_bytes())
                .map_err(map_error)?
                .number()
        };
        Ok(self.fs.upgrade().unwrap().inode(number)?)
    }
    fn create(
        &self,
        _dentry: &Dentry,
        name: &str,
        type_: InodeType,
        mode: InodeMode,
    ) -> Result<Arc<dyn Inode>> {
        let metadata = Self::creation(mode)?;
        self.create_with(|writer| match type_ {
            InodeType::File => writer.create_file(self.number, name.as_bytes(), metadata),
            InodeType::Dir => writer.create_directory(self.number, name.as_bytes(), metadata),
            InodeType::Socket => {
                writer.create_special(self.number, name.as_bytes(), SpecialFile::Socket, metadata)
            }
            _ => Err(aster_ext4::Error::InvalidInput),
        })
    }
    fn create_symlink(
        &self,
        _dentry: &Dentry,
        name: &str,
        target: &str,
        mode: InodeMode,
    ) -> Result<Arc<dyn Inode>> {
        if target.len() > PATH_MAX {
            return_errno!(Errno::ENAMETOOLONG);
        }
        let metadata = Self::creation(mode)?;
        self.create_with(|writer| {
            writer.create_symlink(self.number, name.as_bytes(), target.as_bytes(), metadata)
        })
    }
    fn create_tmpfile(
        &self,
        _dentry: &Dentry,
        mode: InodeMode,
        linkability: HardLinkability,
    ) -> Result<Arc<dyn Inode>> {
        let metadata = Self::creation(mode)?;
        let raw = self.volume().lock().mutate(&[self.number], |writer| {
            writer.create_tmpfile(self.number, metadata)
        })?;
        let inode = self.fs.upgrade().unwrap().inode(raw.number())?;
        inode.linkable.store(
            linkability == HardLinkability::Linkable,
            core::sync::atomic::Ordering::Release,
        );
        Ok(inode)
    }
    fn mknod(
        &self,
        _dentry: &Dentry,
        name: &str,
        mode: InodeMode,
        type_: MknodType,
    ) -> Result<Arc<dyn Inode>> {
        let kind = match type_ {
            MknodType::NamedPipe => SpecialFile::NamedPipe,
            MknodType::CharDevice(encoded) | MknodType::BlockDevice(encoded) => {
                let (major, minor) = device_id::decode_device_numbers(encoded);
                let device = aster_ext4::DeviceNumber::new(major, minor).map_err(map_error)?;
                if matches!(type_, MknodType::CharDevice(_)) {
                    SpecialFile::Character(device)
                } else {
                    SpecialFile::Block(device)
                }
            }
        };
        let metadata = Self::creation(mode)?;
        self.create_with(|writer| {
            writer.create_special(self.number, name.as_bytes(), kind, metadata)
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
    fn link(&self, _dentry: &Dentry, old: &Dentry, name: &str) -> Result<()> {
        let old = self.same_fs(old.inode().as_ref())?;
        if !old.linkable.load(core::sync::atomic::Ordering::Acquire) {
            return_errno!(Errno::ENOENT);
        }
        let changed = timestamp(Self::now())?;
        self.volume()
            .lock()
            .mutate(&[self.number, old.number], |writer| {
                writer.link(self.number, name.as_bytes(), old.number, changed)
            })
            .map(|_| ())
    }
    fn unlink(&self, child: &Dentry) -> Result<()> {
        self.remove_child(child, Removal::File)
    }
    fn rmdir(&self, child: &Dentry) -> Result<()> {
        self.remove_child(child, Removal::Directory)
    }
    fn rename(
        &self,
        old: &Dentry,
        new_dir: &Dentry,
        name: &str,
        replaced: Option<&Arc<dyn Inode>>,
        mode: RenameMode,
    ) -> Result<()> {
        if mode == RenameMode::Exchange {
            return_errno!(Errno::EOPNOTSUPP);
        }
        let target = self.same_fs(new_dir.inode().as_ref())?;
        let source = self.same_fs(old.inode().as_ref())?;
        if mode == RenameMode::NoReplace && replaced.is_some() {
            return_errno!(Errno::EEXIST);
        }
        let mut numbers = vec![self.number, target.number, source.number];
        if let Some(replaced) = replaced {
            numbers.push(self.same_fs(replaced.as_ref())?.number);
        }
        let changed = timestamp(Self::now())?;
        self.volume().lock().mutate(&numbers, |writer| {
            writer.rename(
                self.number,
                old.name().as_bytes(),
                target.number,
                name.as_bytes(),
                changed,
            )
        })?;
        Ok(())
    }
    fn read_link(&self) -> Result<SymbolicLink> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }
        if self.size() > PATH_MAX {
            return_errno!(Errno::ENAMETOOLONG);
        }
        let volume = self.volume().lock();
        let reader = volume.reader()?;
        let raw = reader.inode(self.number).map_err(map_error)?;
        let bytes = reader.read_link(&raw).map_err(map_error)?;
        Ok(SymbolicLink::Plain(
            String::from_utf8(bytes).map_err(|_| Error::new(Errno::EIO))?,
        ))
    }
    fn get_xattr(&self, name: XattrName, writer: &mut VmWriter) -> Result<usize> {
        self.get_attribute(name, writer)
    }
    fn list_xattr(&self, namespace: XattrNamespace, writer: &mut VmWriter) -> Result<usize> {
        self.list_attributes(namespace, writer)
    }
    fn set_xattr(
        &self,
        _dentry: &Dentry,
        name: XattrName,
        reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()> {
        self.set_attribute(name, reader, flags)
    }
    fn remove_xattr(&self, _dentry: &Dentry, name: XattrName) -> Result<()> {
        self.remove_attribute(name)
    }
}

struct DataBackend {
    volume: Arc<Mutex<Volume>>,
    number: u32,
}

impl PageCacheBackend for DataBackend {
    fn read_page_async(
        &self,
        idx: usize,
        page: LockedCachePage,
        _batch: &mut IoBatch,
    ) -> Result<()> {
        read_cache_page(idx, page, |offset, bytes| {
            let volume = self.volume.lock();
            if offset >= volume.metadata(self.number).size {
                return_errno!(Errno::EINVAL);
            }
            let reader = volume.reader()?;
            let raw = reader.inode(self.number).map_err(map_error)?;
            reader
                .read_at(&raw, offset as u64, bytes)
                .map_err(map_error)?;
            Ok(())
        })
    }
    fn write_page_async(
        &self,
        idx: usize,
        page: LockedCachePage,
        _batch: &mut IoBatch,
    ) -> Result<()> {
        let result = (|| {
            let offset = idx
                .checked_mul(PAGE_SIZE)
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            let mut bytes = vec![0; PAGE_SIZE];
            page.read_bytes(0, &mut bytes)?;
            let mut volume = self.volume.lock();
            let metadata = volume.metadata(self.number);
            if offset >= metadata.size {
                return_errno!(Errno::EINVAL);
            }
            let count = PAGE_SIZE.min(metadata.size - offset);
            let changed = timestamp(metadata.last_modify_at)?;
            volume.mutate(&[self.number], |writer| {
                let mut copied = 0;
                while copied < count {
                    let written = writer.write_at(
                        self.number,
                        (offset + copied) as u64,
                        &bytes[copied..count],
                        changed,
                    )?;
                    if written == 0 {
                        return Err(aster_ext4::Error::Io(io_util::IoError::Failed));
                    }
                    copied += written;
                }
                Ok(())
            })
        })();
        if result.is_err() {
            page.set_dirty();
        }
        result
    }
}
