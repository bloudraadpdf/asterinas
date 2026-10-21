// SPDX-License-Identifier: MPL-2.0

//! Mount ownership and metadata shared with page-cache backends.

use core::time::Duration;

use aster_ext4::{Ext4 as Reader, Geometry, MetadataUpdate, MountLimits, Timestamp, WritableExt4};
use device_id::DeviceId;
use io_util::metadata_cache::MetadataCache;

use super::map_error;
use crate::{
    fs::{
        file::{InodeMode, InodeType},
        utils::block_io::BlockIo,
        vfs::{file_system::FsFlags, inode::Metadata},
    },
    prelude::*,
    process::{Gid, Uid},
};

enum DiskVolume {
    ReadOnly(Box<Reader<MetadataCache<BlockIo>>>),
    Writable(Box<WritableExt4<MetadataCache<BlockIo>>>),
}

struct CachedInode {
    flags: u32,
    metadata: Metadata,
    dirty_times: bool,
}

pub(super) struct Volume {
    disk: Option<DiskVolume>,
    device_id: DeviceId,
    cached: BTreeMap<u32, CachedInode>,
    failure: Option<Errno>,
}

impl Volume {
    pub(super) fn open(device: BlockIo, flags: FsFlags) -> Result<Self> {
        let device_id = device.0.id();
        let device = if flags.contains(FsFlags::DIRSYNC) {
            MetadataCache::new(device)
        } else {
            MetadataCache::writeback(device)
        };
        let disk = if flags.contains(FsFlags::RDONLY) {
            DiskVolume::ReadOnly(Box::new(Reader::open(device).map_err(map_error)?))
        } else {
            DiskVolume::Writable(Box::new(
                WritableExt4::open(
                    device,
                    MountLimits {
                        journal_blocks: 32768,
                        transactions: 128,
                        revocations: 8192,
                        mapping_entries: 4096,
                        orphan_inodes: 4096,
                        metadata_blocks: 64,
                        data_blocks: 4,
                        allocation_groups: 4096,
                        directory_depth: 128,
                    },
                )
                .map_err(map_error)?,
            ))
        };
        Ok(Self {
            disk: Some(disk),
            device_id,
            cached: BTreeMap::new(),
            failure: None,
        })
    }

    pub(super) fn geometry(&self) -> &Geometry {
        match self.disk.as_ref().unwrap() {
            DiskVolume::ReadOnly(reader) => reader.geometry(),
            DiskVolume::Writable(writer) => writer.geometry(),
        }
    }

    pub(super) fn reader(&self) -> Result<&Reader<MetadataCache<BlockIo>>> {
        self.check()?;
        match self.disk.as_ref().unwrap() {
            DiskVolume::ReadOnly(reader) => Ok(reader),
            DiskVolume::Writable(writer) => writer.reader().map_err(map_error),
        }
    }

    pub(super) fn writer(&mut self) -> Result<&mut WritableExt4<MetadataCache<BlockIo>>> {
        self.check()?;
        match self.disk.as_mut().unwrap() {
            DiskVolume::ReadOnly(_) => Err(Error::new(Errno::EROFS)),
            DiskVolume::Writable(writer) => Ok(writer),
        }
    }

    fn check(&self) -> Result<()> {
        match self.failure {
            Some(error) => Err(Error::new(error)),
            None => Ok(()),
        }
    }

    pub(super) fn retain_error(&mut self, error: &Error) {
        self.failure.get_or_insert(error.error());
    }

    pub(super) fn flags(&self, number: u32) -> u32 {
        self.cached[&number].flags
    }

    pub(super) fn metadata(&self, number: u32) -> Metadata {
        self.cached[&number].metadata
    }

    pub(super) fn load(&mut self, number: u32) -> Result<Metadata> {
        let raw = self.reader()?.inode(number).map_err(map_error)?;
        let metadata = self.decode(&raw)?;
        self.cached.insert(
            number,
            CachedInode {
                flags: raw.flags(),
                metadata,
                dirty_times: false,
            },
        );
        Ok(metadata)
    }

    fn decode(&self, raw: &aster_ext4::Inode) -> Result<Metadata> {
        let size = usize::try_from(raw.size()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        if size > isize::MAX as usize {
            return_errno!(Errno::EOVERFLOW);
        }
        let self_dev_id = raw
            .device_number()
            .map(|device| {
                DeviceId::from_encoded_u64(device_id::encode_device_numbers(
                    device.major(),
                    device.minor(),
                ))
                .ok_or_else(|| Error::new(Errno::EOVERFLOW))
            })
            .transpose()?;
        Ok(Metadata {
            ino: u64::from(raw.number()),
            size,
            optimal_block_size: self.geometry().block_size() as usize,
            nr_sectors_allocated: usize::try_from(raw.allocated_sectors(self.geometry()))
                .map_err(|_| Error::new(Errno::EOVERFLOW))?,
            last_access_at: duration(raw.access_time().map_err(map_error)?)?,
            last_modify_at: duration(raw.modify_time().map_err(map_error)?)?,
            last_meta_change_at: duration(raw.change_time().map_err(map_error)?)?,
            type_: InodeType::from_raw_mode(raw.mode())?,
            mode: InodeMode::from_bits_truncate(raw.mode()),
            nr_hard_links: usize::from(raw.links()),
            uid: Uid::new(raw.uid()),
            gid: Gid::new(raw.gid()),
            container_dev_id: self.device_id,
            self_dev_id,
            birth_at: raw
                .birth_time()
                .map_err(map_error)?
                .map(duration)
                .transpose()?,
        })
    }

    pub(super) fn refresh(&mut self, number: u32) -> Result<()> {
        if self.cached.contains_key(&number) {
            self.load(number)?;
        }
        Ok(())
    }

    pub(super) fn change_times(&mut self, number: u32, change: impl FnOnce(&mut Metadata)) {
        if matches!(self.disk.as_ref(), Some(DiskVolume::ReadOnly(_))) {
            return;
        }
        let cached = self.cached.get_mut(&number).unwrap();
        change(&mut cached.metadata);
        cached.dirty_times = true;
    }

    pub(super) fn flush_times(&mut self, number: u32) -> Result<()> {
        let Some(cached) = self.cached.get(&number).filter(|cached| cached.dirty_times) else {
            return Ok(());
        };
        let metadata = cached.metadata;
        let mut update = MetadataUpdate::new(timestamp(metadata.last_meta_change_at)?);
        update.accessed = Some(timestamp(metadata.last_access_at)?);
        update.modified = Some(timestamp(metadata.last_modify_at)?);
        self.writer()?
            .set_metadata(number, update)
            .map_err(map_error)?;
        self.cached.get_mut(&number).unwrap().dirty_times = false;
        Ok(())
    }

    pub(super) fn mutate<T>(
        &mut self,
        numbers: &[u32],
        action: impl FnOnce(
            &mut WritableExt4<MetadataCache<BlockIo>>,
        ) -> core::result::Result<T, aster_ext4::Error>,
    ) -> Result<T> {
        for &number in numbers {
            self.flush_times(number)?;
        }
        let result = action(self.writer()?).map_err(map_error)?;
        for &number in numbers {
            self.refresh(number)?;
        }
        Ok(result)
    }

    pub(super) fn sync(&mut self) -> Result<()> {
        self.check()?;
        let mut after = None;
        loop {
            let start = after.map_or(core::ops::Bound::Unbounded, core::ops::Bound::Excluded);
            let number = self
                .cached
                .range((start, core::ops::Bound::Unbounded))
                .next()
                .map(|(&number, _)| number);
            let Some(number) = number else {
                break;
            };
            self.flush_times(number)?;
            after = Some(number);
        }
        match self.disk.as_mut().unwrap() {
            DiskVolume::ReadOnly(_) => Ok(()),
            DiskVolume::Writable(writer) => writer.sync().map_err(map_error),
        }
    }

    pub(super) fn release(&mut self, number: u32) -> Result<()> {
        if self.cached[&number].metadata.nr_hard_links == 0 {
            self.writer()?.reap(number).map_err(map_error)?;
        } else {
            self.flush_times(number)?;
        }
        self.cached.remove(&number);
        Ok(())
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        if let Some(DiskVolume::Writable(writer)) = self.disk.take()
            && self.failure.is_none()
            && self.cached.is_empty()
            && let Err(error) = writer.close()
        {
            error!("ext4 clean release failed: {:?}", error);
        }
    }
}

pub(super) fn timestamp(time: Duration) -> Result<Timestamp> {
    let seconds = i64::try_from(time.as_secs()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    Timestamp::new(seconds, time.subsec_nanos()).map_err(|_| Error::new(Errno::EOVERFLOW))
}

fn duration(time: Timestamp) -> Result<Duration> {
    let seconds = u64::try_from(time.seconds()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
    Ok(Duration::new(seconds, time.nanoseconds()))
}
