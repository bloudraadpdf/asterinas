// SPDX-License-Identifier: MPL-2.0

//! Checked byte I/O for disk filesystem libraries.

use aster_block::BlockDevice;
#[cfg(feature = "fs_ext4")]
use aster_block::bio::BioStatus;
use io_util::{IoError, ReadAt};
use ostd::mm::VmIo;

use crate::prelude::Arc;

pub(in crate::fs) struct BlockIo(pub Arc<dyn BlockDevice>);

impl BlockIo {
    pub(in crate::fs) fn check_magic<const N: usize>(
        &self,
        offset: u64,
        magic: [u8; N],
    ) -> crate::prelude::Result<()> {
        use crate::prelude::{Errno, Error};
        let mut found = [0; N];
        self.read_exact_at(offset, &mut found)
            .map_err(|_| Error::new(Errno::EIO))?;
        if found != magic {
            return Err(Error::new(Errno::EINVAL));
        }
        Ok(())
    }
}

impl ReadAt for BlockIo {
    fn len(&self) -> u64 {
        (self.0.metadata().nr_sectors as u64).saturating_mul(aster_block::SECTOR_SIZE as u64)
    }

    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError> {
        let offset = usize::try_from(offset).map_err(|_| IoError::Failed)?;
        self.0
            .read_bytes(offset, buffer)
            .map_err(|_| IoError::Failed)
    }
}

#[cfg(feature = "fs_ext4")]
impl io_util::WriteAt for BlockIo {
    fn is_read_only(&self) -> bool {
        self.0.metadata().is_read_only
    }

    fn write_all_at(&self, offset: u64, buffer: &[u8]) -> Result<(), IoError> {
        if self.is_read_only() {
            return Err(IoError::Unsupported);
        }
        let offset = usize::try_from(offset).map_err(|_| IoError::Failed)?;
        self.0
            .write_bytes(offset, buffer)
            .map_err(|_| IoError::Failed)
    }

    fn flush(&self) -> Result<(), IoError> {
        match self.0.sync().map_err(|_| IoError::Failed)? {
            BioStatus::Complete => Ok(()),
            BioStatus::NotSupported => Err(IoError::Unsupported),
            BioStatus::NoSpace => Err(IoError::OutOfSpace),
            _ => Err(IoError::Failed),
        }
    }
}

pub(in crate::fs) fn read_cache_page(
    index: usize,
    page: crate::vm::page_cache::LockedCachePage,
    read_fn: impl FnOnce(usize, &mut [u8]) -> crate::prelude::Result<()>,
) -> crate::prelude::Result<()> {
    use crate::prelude::{Errno, Error, PAGE_SIZE, vec};

    let offset = index
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| Error::new(Errno::EINVAL))?;
    let mut bytes = vec![0; PAGE_SIZE];
    read_fn(offset, &mut bytes)?;
    page.write_bytes(0, &bytes)?;
    page.set_up_to_date();
    Ok(())
}
