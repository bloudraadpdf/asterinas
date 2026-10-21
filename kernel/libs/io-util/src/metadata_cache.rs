// SPDX-License-Identifier: MPL-2.0

//! Bounded, write-through caching for explicitly identified filesystem metadata.

extern crate alloc;

use alloc::{
    collections::{BTreeMap, VecDeque},
    vec,
    vec::Vec,
};
use core::cell::RefCell;

use crate::{IoError, ReadAt, WriteAt};

const BLOCK_SIZE: usize = 4096;
const CAPACITY: usize = 256;

#[derive(Default)]
struct Blocks {
    data: BTreeMap<u64, Vec<u8>>,
    order: VecDeque<u64>,
}

/// Caches at most 1 MiB of metadata; ordinary reads bypass the cache.
pub struct MetadataCache<D> {
    device: D,
    blocks: RefCell<Blocks>,
}

impl<D> MetadataCache<D> {
    /// Takes exclusive ownership of the backing device.
    pub fn new(device: D) -> Self {
        Self {
            device,
            blocks: RefCell::new(Blocks::default()),
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
                    let oldest = blocks.order.pop_front().unwrap();
                    blocks.data.remove(&oldest);
                }
                blocks.data.insert(block, data);
                blocks.order.push_back(block);
            }
            let data = &blocks.data[&block];
            let length = (data.len() - within).min(buffer.len() - copied);
            buffer[copied..copied + length].copy_from_slice(&data[within..within + length]);
            copied += length;
        }
        Ok(())
    }
}

impl<D: WriteAt> WriteAt for MetadataCache<D> {
    fn is_read_only(&self) -> bool {
        self.device.is_read_only()
    }

    fn write_all_at(&self, offset: u64, buffer: &[u8]) -> Result<(), IoError> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= self.len())
            .ok_or(IoError::Failed)?;
        if let Err(error) = self.device.write_all_at(offset, buffer) {
            *self.blocks.borrow_mut() = Blocks::default();
            return Err(error);
        }
        if buffer.is_empty() {
            return Ok(());
        }
        let mut blocks = self.blocks.borrow_mut();
        for (&block, data) in blocks
            .data
            .range_mut(offset / BLOCK_SIZE as u64..=(end - 1) / BLOCK_SIZE as u64)
        {
            let start = block * BLOCK_SIZE as u64;
            let first = offset.max(start);
            let last = end.min(start + data.len() as u64);
            data[(first - start) as usize..(last - start) as usize]
                .copy_from_slice(&buffer[(first - offset) as usize..(last - offset) as usize]);
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), IoError> {
        self.device.flush()
    }
}
