// SPDX-License-Identifier: MPL-2.0

//! Direct file I/O through the journalled backend, without cached file pages.

use align_ext::AlignExt;

use super::*;

fn check_alignment(offset: usize, length: usize, address: usize) -> Result<()> {
    if !(offset | length | address).is_multiple_of(aster_block::SECTOR_SIZE) {
        return_errno!(Errno::EINVAL);
    }
    Ok(())
}

impl Ext4Inode {
    pub(super) fn read_direct_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if writer.avail() == 0 {
            return Ok(0);
        }
        check_alignment(offset, writer.avail(), writer.cursor() as usize)?;
        let cache = self.cache.lock();
        let length = writer.avail().min(self.size().saturating_sub(offset));
        if length == 0 {
            return Ok(0);
        }
        cache.flush_range(offset..offset + length)?;
        let mut bytes = vec![0; PAGE_SIZE];
        let mut read = 0;
        let result: Result<()> = (|| {
            while read < length {
                let count = (length - read).min(bytes.len());
                let count = {
                    let volume = self.volume().lock();
                    let disk = volume.reader()?;
                    let inode = disk.inode(self.number).map_err(map_error)?;
                    disk.read_at(&inode, (offset + read) as u64, &mut bytes[..count])
                        .map_err(map_error)?
                };
                if count == 0 {
                    break;
                }
                let before = writer.avail();
                let result = writer.write_fallible(&mut VmReader::from(&bytes[..count]));
                read += before - writer.avail();
                result.map_err(|(error, _)| error)?;
            }
            Ok(())
        })();
        if read != 0 {
            return Ok(read);
        }
        result.map(|()| 0)
    }

    pub(super) fn write_direct_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if reader.remain() == 0 {
            return Ok(0);
        }
        check_alignment(offset, reader.remain(), reader.cursor() as usize)?;
        let end = offset
            .checked_add(reader.remain())
            .filter(|&end| end <= isize::MAX as usize)
            .ok_or_else(|| Error::new(Errno::EFBIG))?;
        let mut cache = self.cache.lock();
        let old_size = self.size();
        {
            let mut volume = self.volume().lock();
            volume.writer()?;
            if volume.flags(self.number) & 0x10 != 0 {
                return_errno!(Errno::EPERM);
            }
            if volume.flags(self.number) & 0x20 != 0 {
                return_errno!(Errno::EOPNOTSUPP);
            }
        }
        let start = offset.min(old_size).align_down(PAGE_SIZE);
        cache.invalidate_range(start..end)?;
        let changed = timestamp(Self::now())?;
        let mut bytes = vec![0; PAGE_SIZE];
        let mut written = 0;
        let result: Result<()> = (|| {
            while reader.remain() != 0 {
                let count = reader.remain().min(bytes.len());
                let (count, fault) =
                    match VmWriter::from(&mut bytes[..count]).write_fallible(reader) {
                        Ok(count) => (count, None),
                        Err((error, count)) => (count, Some(error)),
                    };
                let mut copied = 0;
                while copied < count {
                    let count = self.volume().lock().mutate(&[self.number], |disk| {
                        disk.write_at(
                            self.number,
                            (offset + written) as u64,
                            &bytes[copied..count],
                            changed,
                        )
                    })?;
                    if count == 0 {
                        return_errno!(Errno::EIO);
                    }
                    copied += count;
                    written += count;
                }
                if let Some(error) = fault {
                    return Err(error.into());
                }
            }
            Ok(())
        })();
        let size = self.size();
        if size > old_size {
            cache.grow_initialized(size);
        }
        if written != 0 {
            // Page faults can repopulate the cache while block I/O is in progress.
            cache.invalidate_range(start..offset + written)?;
            return Ok(written);
        }
        result.map(|()| 0)
    }
}
