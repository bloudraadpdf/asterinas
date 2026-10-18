// SPDX-License-Identifier: MPL-2.0

//! Incremental extent insertion and bounded path splitting.

use alloc::{vec, vec::Vec};

use io_util::ReadAt;

use super::Transaction;
use crate::{
    Error, Inode, Result,
    extent::{EXTENTS_FLAG, Extent, ExtentKind, Node, Record},
    u16_at, u32_at,
};

struct Frame {
    block: Option<u64>,
    parent_slot: usize,
    node: Node,
}

impl<D: ReadAt> Transaction<'_, D> {
    /// Returns a physical block and whether its prior contents must be zeroed.
    pub(super) fn ensure_file_block(
        &mut self,
        inode: &mut Inode,
        logical: u32,
    ) -> Result<(u64, bool)> {
        if u32_at(&inode.raw, 32)? & EXTENTS_FLAG == 0 {
            return self
                .view()
                .file_block(inode, logical)?
                .map(|physical| (physical, false))
                .ok_or(Error::Unsupported);
        }
        let mut path = self.extent_path(inode, logical)?;
        let leaf = &mut path.last_mut().ok_or(Error::Corrupt)?.node;
        let index = leaf
            .records
            .partition_point(|record| record.logical() <= logical);
        let covering = index
            .checked_sub(1)
            .and_then(|index| match leaf.records[index] {
                Record::Leaf(extent)
                    if u64::from(logical)
                        < u64::from(extent.logical) + u64::from(extent.blocks) =>
                {
                    Some((index, extent))
                }
                _ => None,
            });
        let physical = if let Some((index, extent)) = covering {
            let within = logical - extent.logical;
            let physical = extent.physical + u64::from(within);
            if extent.kind == ExtentKind::Written {
                return Ok((physical, false));
            }
            let mut replacements = Vec::with_capacity(3);
            if within > 0 {
                replacements.push(Record::Leaf(Extent {
                    blocks: within,
                    ..extent
                }));
            }
            replacements.push(Record::Leaf(Extent {
                logical,
                physical,
                blocks: 1,
                kind: ExtentKind::Written,
            }));
            let remaining = extent.blocks - within - 1;
            if remaining > 0 {
                replacements.push(Record::Leaf(Extent {
                    logical: logical + 1,
                    physical: physical + 1,
                    blocks: remaining,
                    kind: ExtentKind::Unwritten,
                }));
            }
            leaf.records.splice(index..index + 1, replacements);
            physical
        } else {
            let preferred = if logical > 0 {
                self.view()
                    .file_block(inode, logical - 1)?
                    .and_then(|block| block.checked_add(1))
            } else {
                None
            };
            let physical = self.allocate_block(preferred)?;
            adjust_inode_blocks(inode, 1, self.geometry.block_size())?;
            leaf.records.insert(
                index,
                Record::Leaf(Extent {
                    logical,
                    physical,
                    blocks: 1,
                    kind: ExtentKind::Written,
                }),
            );
            physical
        };
        merge_leaf(leaf)?;
        self.persist_extent_path(inode, path)?;
        Ok((physical, true))
    }

    pub(super) fn trim_file_tail(&mut self, inode: &mut Inode) -> Result<bool> {
        if inode.is_special() {
            if inode.has_data_allocations(&self.geometry)? {
                return Err(Error::Corrupt);
            }
            return Ok(false);
        }
        if inode.is_inline_symlink(&self.geometry)? {
            return Ok(false);
        }
        if u32_at(&inode.raw, 32)? & EXTENTS_FLAG == 0 {
            return Err(Error::Unsupported);
        }
        let keep = inode.size().div_ceil(u64::from(self.geometry.block_size()));
        let mut path = self.extent_path(inode, u32::MAX)?;
        let leaf = &mut path.last_mut().ok_or(Error::Corrupt)?.node;
        let Some(Record::Leaf(extent)) = leaf.records.last().copied() else {
            return Ok(false);
        };
        let end = u64::from(extent.logical) + u64::from(extent.blocks);
        if end <= keep {
            return Ok(false);
        }
        let physical_end = extent.physical + u64::from(extent.blocks);
        let group_start = self.geometry.group_start(0)
            + ((physical_end - 1 - self.geometry.group_start(0))
                / u64::from(self.geometry.blocks_per_group()))
                * u64::from(self.geometry.blocks_per_group());
        let remove =
            (end - keep.max(u64::from(extent.logical))).min(physical_end - group_start) as u32;
        self.free_blocks(physical_end - u64::from(remove), remove)?;
        adjust_inode_blocks(inode, -i64::from(remove), self.geometry.block_size())?;
        let leaf = &mut path.last_mut().ok_or(Error::Corrupt)?.node;
        if remove == extent.blocks {
            leaf.records.pop();
        } else {
            *leaf.records.last_mut().ok_or(Error::Corrupt)? = Record::Leaf(Extent {
                blocks: extent.blocks - remove,
                ..extent
            });
        }
        while let Some(mut frame) = path.pop() {
            if let Some(block) = frame.block {
                if frame.node.records.is_empty() {
                    self.free_blocks(block, 1)?;
                    adjust_inode_blocks(inode, -1, self.geometry.block_size())?;
                    path.last_mut()
                        .ok_or(Error::Corrupt)?
                        .node
                        .records
                        .remove(frame.parent_slot);
                } else {
                    self.store_extent_node(block, &frame.node, inode)?;
                }
            } else {
                if frame.node.records.is_empty() {
                    frame.node = Node::new(0, 4, Vec::new());
                }
                while frame.node.depth > 0 && frame.node.records.len() == 1 {
                    let Record::Index { logical, physical } = frame.node.records[0] else {
                        return Err(Error::Corrupt);
                    };
                    let mut child = Node::external(
                        &self.view().read_block(physical)?,
                        inode,
                        &self.geometry,
                        frame.node.depth - 1,
                        u64::from(logical)..u64::from(u32::MAX) + 1,
                    )?;
                    if child.records.len() > 4 {
                        break;
                    }
                    self.free_blocks(physical, 1)?;
                    adjust_inode_blocks(inode, -1, self.geometry.block_size())?;
                    child.capacity = 4;
                    frame.node = child;
                }
                let raw = frame.node.serialize(inode, &self.geometry, 60)?;
                inode.raw[40..100].copy_from_slice(&raw);
            }
        }
        Ok(true)
    }

    fn extent_path(&self, inode: &Inode, logical: u32) -> Result<Vec<Frame>> {
        let root = Node::root(inode, &self.geometry)?;
        let mut path = vec![Frame {
            block: None,
            parent_slot: 0,
            node: root,
        }];
        let mut upper = u64::from(u32::MAX) + 1;
        loop {
            let node = &path.last().ok_or(Error::Corrupt)?.node;
            if node.depth == 0 {
                return Ok(path);
            }
            let slot = node
                .records
                .partition_point(|record| record.logical() <= logical)
                .saturating_sub(1);
            let Record::Index {
                logical: lower,
                physical,
            } = node.records[slot]
            else {
                return Err(Error::Corrupt);
            };
            if let Some(record) = node.records.get(slot + 1) {
                upper = u64::from(record.logical());
            }
            self.check_file_block(physical)?;
            let child = Node::external(
                &self.view().read_block(physical)?,
                inode,
                &self.geometry,
                node.depth - 1,
                u64::from(lower)..upper,
            )?;
            path.push(Frame {
                block: Some(physical),
                parent_slot: slot,
                node: child,
            });
        }
    }

    fn persist_extent_path(&mut self, inode: &mut Inode, mut path: Vec<Frame>) -> Result<()> {
        while let Some(mut frame) = path.pop() {
            let key = frame.node.records.first().ok_or(Error::Corrupt)?.logical();
            let Some(block) = frame.block else {
                let root = if frame.node.records.len() > usize::from(frame.node.capacity) {
                    if frame.node.depth == 5 {
                        return Err(Error::ResourceLimit);
                    }
                    let physical = self.allocate_block(None)?;
                    adjust_inode_blocks(inode, 1, self.geometry.block_size())?;
                    frame.node.capacity = Node::external_capacity(&self.geometry);
                    self.store_extent_node(physical, &frame.node, inode)?;
                    Node::new(
                        frame.node.depth + 1,
                        4,
                        vec![Record::Index {
                            logical: key,
                            physical,
                        }],
                    )
                } else {
                    frame.node
                };
                let raw = root.serialize(inode, &self.geometry, 60)?;
                inode.raw[40..100].copy_from_slice(&raw);
                return Ok(());
            };
            let right = if frame.node.records.len() > usize::from(frame.node.capacity) {
                let records = frame.node.records.split_off(frame.node.records.len() / 2);
                let physical = self.allocate_block(None)?;
                adjust_inode_blocks(inode, 1, self.geometry.block_size())?;
                let right = Node::new(
                    frame.node.depth,
                    Node::external_capacity(&self.geometry),
                    records,
                );
                let key = right.records.first().ok_or(Error::Corrupt)?.logical();
                self.store_extent_node(physical, &right, inode)?;
                Some(Record::Index {
                    logical: key,
                    physical,
                })
            } else {
                None
            };
            self.store_extent_node(block, &frame.node, inode)?;
            let parent = &mut path.last_mut().ok_or(Error::Corrupt)?.node;
            parent.records[frame.parent_slot] = Record::Index {
                logical: key,
                physical: block,
            };
            if let Some(right) = right {
                parent.records.insert(frame.parent_slot + 1, right);
            }
        }
        Err(Error::Corrupt)
    }

    fn store_extent_node(&mut self, block: u64, node: &Node, inode: &Inode) -> Result<()> {
        let raw = node.serialize(inode, &self.geometry, self.geometry.block_size() as usize)?;
        self.metadata_block(block)?.copy_from_slice(&raw);
        Ok(())
    }
}

fn merge_leaf(node: &mut Node) -> Result<()> {
    let mut index = 0;
    while index + 1 < node.records.len() {
        let (Record::Leaf(left), Record::Leaf(right)) =
            (node.records[index], node.records[index + 1])
        else {
            return Err(Error::Corrupt);
        };
        let maximum = if left.kind == ExtentKind::Written {
            32768
        } else {
            32767
        };
        if left.kind == right.kind
            && u64::from(left.logical) + u64::from(left.blocks) == u64::from(right.logical)
            && left.physical + u64::from(left.blocks) == right.physical
            && left.blocks + right.blocks <= maximum
        {
            node.records[index] = Record::Leaf(Extent {
                blocks: left.blocks + right.blocks,
                ..left
            });
            node.records.remove(index + 1);
        } else {
            index += 1;
        }
    }
    Ok(())
}

pub(super) fn adjust_inode_blocks(inode: &mut Inode, blocks: i64, block_size: u32) -> Result<()> {
    let count = u64::from(u32_at(&inode.raw, 28)?) | (u64::from(u16_at(&inode.raw, 116)?) << 32);
    let units = if u32_at(&inode.raw, 32)? & 0x0004_0000 != 0 {
        blocks
    } else {
        blocks
            .checked_mul(i64::from(block_size / 512))
            .ok_or(Error::Corrupt)?
    };
    let count = count
        .checked_add_signed(units)
        .filter(|count| *count < 1 << 48)
        .ok_or(Error::Corrupt)?;
    inode.raw[28..32].copy_from_slice(&(count as u32).to_le_bytes());
    inode.raw[116..118].copy_from_slice(&((count >> 32) as u16).to_le_bytes());
    Ok(())
}
