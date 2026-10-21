// SPDX-License-Identifier: MPL-2.0

//! Bounded metadata caching with journal-owned staging.

extern crate alloc;

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use crate::{IoError, ReadAt, WriteAt, checksum};

const BLOCK_SIZE: usize = 4096;
const CAPACITY: usize = 256;

struct CachedBlock {
    bytes: Vec<u8>,
    checksum: Option<CachedChecksum>,
}

struct CachedChecksum {
    offset: usize,
    length: usize,
    seed: u32,
    value: u32,
}

#[derive(Default)]
struct Blocks {
    data: BTreeMap<u64, CachedBlock>,
    order: VecDeque<u64>,
    staged: BTreeSet<u64>,
}

/// Caches at most 1 MiB of metadata; ordinary reads bypass the cache.
pub struct MetadataCache<D> {
    device: D,
    blocks: RefCell<Blocks>,
    writeback: bool,
}

impl<D> MetadataCache<D> {
    /// Takes exclusive ownership of the backing device.
    pub fn new(device: D) -> Self {
        Self {
            device,
            blocks: RefCell::new(Blocks::default()),
            writeback: false,
        }
    }

    /// Allows the journal owner to stage up to 256 KiB of metadata.
    pub fn writeback(device: D) -> Self {
        Self {
            writeback: true,
            ..Self::new(device)
        }
    }

    fn update_cached(&self, offset: u64, buffer: &[u8]) {
        if buffer.is_empty() {
            return;
        }
        let end = offset + buffer.len() as u64;
        let mut blocks = self.blocks.borrow_mut();
        for (&block, data) in blocks
            .data
            .range_mut(offset / BLOCK_SIZE as u64..=(end - 1) / BLOCK_SIZE as u64)
        {
            data.checksum = None;
            let start = block * BLOCK_SIZE as u64;
            let first = offset.max(start);
            let last = end.min(start + data.bytes.len() as u64);
            data.bytes[(first - start) as usize..(last - start) as usize]
                .copy_from_slice(&buffer[(first - offset) as usize..(last - offset) as usize]);
        }
    }
}

impl<D: ReadAt> ReadAt for MetadataCache<D> {
    fn len(&self) -> u64 {
        self.device.len()
    }

    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError> {
        self.device.read_exact_at(offset, buffer)
    }

    fn metadata_crc32c(&self, offset: u64, bytes: &[u8], seed: u32) -> u32 {
        let within = (offset % BLOCK_SIZE as u64) as usize;
        let mut blocks = self.blocks.borrow_mut();
        if let Some(block) = blocks.data.get_mut(&(offset / BLOCK_SIZE as u64))
            && block
                .bytes
                .get(within..)
                .and_then(|tail| tail.get(..bytes.len()))
                == Some(bytes)
        {
            if let Some(cached) = &block.checksum
                && cached.offset == within
                && cached.length == bytes.len()
                && cached.seed == seed
            {
                return cached.value;
            }
            let value = checksum::crc32c(seed, bytes);
            block.checksum = Some(CachedChecksum {
                offset: within,
                length: bytes.len(),
                seed,
                value,
            });
            return value;
        }
        checksum::crc32c(seed, bytes)
    }

    fn read_metadata_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), IoError> {
        if offset
            .checked_add(buffer.len() as u64)
            .is_none_or(|end| end > self.len())
        {
            return Err(IoError::Failed);
        }
        let mut blocks = self.blocks.borrow_mut();
        let mut copied = 0;
        while copied < buffer.len() {
            let position = offset + copied as u64;
            let block = position / BLOCK_SIZE as u64;
            let within = (position % BLOCK_SIZE as u64) as usize;
            if !blocks.data.contains_key(&block) {
                let start = block * BLOCK_SIZE as u64;
                let size = (self.len() - start).min(BLOCK_SIZE as u64) as usize;
                let mut data = vec![0; size];
                self.device.read_exact_at(start, &mut data)?;
                if blocks.data.len() == CAPACITY {
                    loop {
                        let oldest = blocks.order.pop_front().unwrap();
                        if blocks.staged.contains(&oldest) {
                            blocks.order.push_back(oldest);
                        } else {
                            blocks.data.remove(&oldest);
                            break;
                        }
                    }
                }
                blocks.data.insert(
                    block,
                    CachedBlock {
                        bytes: data,
                        checksum: None,
                    },
                );
                blocks.order.push_back(block);
            }
            let data = &blocks.data[&block].bytes;
            let length = (data.len() - within).min(buffer.len() - copied);
            buffer[copied..copied + length].copy_from_slice(&data[within..within + length]);
            copied += length;
        }
        Ok(())
    }
}

impl<D: WriteAt> WriteAt for MetadataCache<D> {
    fn metadata_writeback_limit(&self) -> usize {
        if self.writeback { 64 * BLOCK_SIZE } else { 0 }
    }

    fn stage_metadata(&self, offset: u64, buffer: &[u8]) -> Result<(), IoError> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= self.len())
            .ok_or(IoError::Failed)?;
        if !self.writeback || buffer.is_empty() {
            return Err(IoError::Unsupported);
        }
        let range = offset / BLOCK_SIZE as u64..=(end - 1) / BLOCK_SIZE as u64;
        {
            let blocks = self.blocks.borrow();
            let added = range
                .clone()
                .filter(|block| !blocks.staged.contains(block))
                .count();
            if blocks.staged.len() + added > 64 {
                return Err(IoError::OutOfSpace);
            }
        }
        let mut byte = [0; 1];
        for block in range.clone() {
            self.read_metadata_exact_at(block * BLOCK_SIZE as u64, &mut byte)?;
        }
        self.update_cached(offset, buffer);
        self.blocks.borrow_mut().staged.extend(range);
        Ok(())
    }

    fn clear_staged_metadata(&self) {
        self.blocks.borrow_mut().staged.clear();
    }

    fn is_read_only(&self) -> bool {
        self.device.is_read_only()
    }

    fn write_all_at(&self, offset: u64, buffer: &[u8]) -> Result<(), IoError> {
        offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= self.len())
            .ok_or(IoError::Failed)?;
        if let Err(error) = self.device.write_all_at(offset, buffer) {
            *self.blocks.borrow_mut() = Blocks::default();
            return Err(error);
        }
        self.update_cached(offset, buffer);
        Ok(())
    }

    fn flush(&self) -> Result<(), IoError> {
        self.device.flush()
    }
}
