// SPDX-License-Identifier: MPL-2.0

//! Checked logical-to-physical block lookup.

use io_util::ReadAt;

use super::{Error, Ext4, Inode, Result, u32_at};

mod node;

pub(super) use node::{Extent, ExtentKind, Node, Record};

pub(super) const EXTENTS_FLAG: u32 = 0x0008_0000;

impl<D: ReadAt> Ext4<D> {
    /// Resolves a file block, returning no physical block for holes and unwritten extents.
    pub fn file_block(&self, inode: &Inode, logical: u32) -> Result<Option<u64>> {
        self.file_block_with_nodes(inode, logical, &mut |_| Ok(()))
    }

    pub(super) fn file_block_with_nodes(
        &self,
        inode: &Inode,
        logical: u32,
        visit_fn: &mut impl FnMut(u64) -> Result<()>,
    ) -> Result<Option<u64>> {
        if u32_at(&inode.raw, 32)? & EXTENTS_FLAG == 0 {
            return self.indirect_file_block(inode, u64::from(logical), visit_fn);
        }
        let mut node = Node::root(inode, &self.geometry)?;
        let mut upper = u64::from(u32::MAX) + 1;
        loop {
            let Some(index) = node
                .records
                .partition_point(|record| record.logical() <= logical)
                .checked_sub(1)
            else {
                return Ok(None);
            };
            match node.records[index] {
                Record::Leaf(extent) => {
                    let within = u64::from(logical - extent.logical);
                    return Ok(
                        if within < u64::from(extent.blocks) && extent.kind == ExtentKind::Written {
                            Some(extent.physical + within)
                        } else {
                            None
                        },
                    );
                }
                Record::Index {
                    logical: start,
                    physical,
                } => {
                    if let Some(record) = node.records.get(index + 1) {
                        upper = u64::from(record.logical());
                    }
                    visit_fn(physical)?;
                    node = Node::external(
                        &self.read_block(physical)?,
                        inode,
                        &self.geometry,
                        node.depth - 1,
                        u64::from(start)..upper,
                    )?;
                }
            }
        }
    }

    fn indirect_file_block(
        &self,
        inode: &Inode,
        mut logical: u64,
        visit_fn: &mut impl FnMut(u64) -> Result<()>,
    ) -> Result<Option<u64>> {
        let pointers = u64::from(self.geometry.block_size() / 4);
        if logical < 12 {
            return self.indirect_pointer(&inode.raw, 40 + logical as usize * 4);
        }
        logical -= 12;
        let mut span = pointers;
        for level in 1..=3 {
            if logical < span {
                let Some(mut physical) =
                    self.indirect_pointer(&inode.raw, 40 + (11 + level) * 4)?
                else {
                    return Ok(None);
                };
                let mut divisor = span / pointers;
                for _ in 0..level {
                    visit_fn(physical)?;
                    let node = self.read_block(physical)?;
                    let index = logical / divisor;
                    logical %= divisor;
                    let Some(next) = self.indirect_pointer(&node, index as usize * 4)? else {
                        return Ok(None);
                    };
                    physical = next;
                    divisor /= pointers;
                }
                return Ok(Some(physical));
            }
            logical -= span;
            span = span.checked_mul(pointers).ok_or(Error::Corrupt)?;
        }
        Err(Error::Corrupt)
    }

    fn indirect_pointer(&self, raw: &[u8], offset: usize) -> Result<Option<u64>> {
        let value = u64::from(u32_at(raw, offset)?);
        if value == 0 {
            return Ok(None);
        }
        self.geometry.block_offset(value)?;
        Ok(Some(value))
    }

    /// Reads file bytes through checked extents or legacy indirect blocks.
    pub fn read_at(&self, inode: &Inode, offset: u64, output: &mut [u8]) -> Result<usize> {
        let size = inode.size();
        if offset >= size {
            return Ok(0);
        }
        let count = (size - offset).min(output.len() as u64) as usize;
        if inode.mode() & 0xf000 == 0xa000 && size <= 60 && u32_at(&inode.raw, 28)? == 0 {
            output[..count]
                .copy_from_slice(&inode.raw[40 + offset as usize..40 + offset as usize + count]);
            return Ok(count);
        }
        let block_size = u64::from(self.geometry.block_size());
        let mut copied = 0;
        while copied < count {
            let position = offset + copied as u64;
            let logical = u32::try_from(position / block_size).map_err(|_| Error::Corrupt)?;
            let within = position % block_size;
            let length = (block_size - within).min((count - copied) as u64) as usize;
            match self.file_block(inode, logical)? {
                Some(block) => self.device.read_exact_at(
                    self.geometry.block_offset(block)? + within,
                    &mut output[copied..copied + length],
                )?,
                None => output[copied..copied + length].fill(0),
            }
            copied += length;
        }
        Ok(count)
    }
}
