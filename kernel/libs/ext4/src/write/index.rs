// SPDX-License-Identifier: MPL-2.0

//! Incremental HTree leaf splitting and index growth in one transaction.

use alloc::{vec, vec::Vec};

use io_util::ReadAt;

use super::{Transaction, directory::LeafEntry};
use crate::{
    Error, Inode, Result,
    directory::{
        hash::name_hash,
        index::{Entry, INDEX_FLAG, Node},
    },
};

struct HashedEntry {
    hash: u32,
    entry: LeafEntry,
}

impl<D: ReadAt> Transaction<'_, D> {
    pub(super) fn index_directory(&mut self, inode: &mut Inode) -> Result<()> {
        let view = self.view();
        let entries = view.leaf_entries(inode, 0)?;
        let parent = view.lookup(inode, b"..")?.number();
        let entries: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.name() != b"." && entry.name() != b"..")
            .map(LeafEntry::from)
            .collect();
        let leaf = self.append_directory_block(inode)?;
        let raw = self.encode_leaf(inode, &entries)?;
        self.store_directory_block(inode, leaf, &raw)?;
        let mut root = Node::interior(
            &self.geometry,
            0,
            vec![Entry {
                hash: 0,
                block: leaf,
            }],
        );
        root.raw[..4].copy_from_slice(&inode.number().to_le_bytes());
        root.raw[4..6].copy_from_slice(&12u16.to_le_bytes());
        root.raw[6] = 1;
        root.raw[7] = if self.geometry.has_directory_types() {
            2
        } else {
            0
        };
        root.raw[8] = b'.';
        root.raw[12..16].copy_from_slice(&parent.to_le_bytes());
        root.raw[16..18].copy_from_slice(&((self.geometry.block_size() - 12) as u16).to_le_bytes());
        root.raw[18] = 2;
        root.raw[19] = root.raw[7];
        root.raw[20..22].copy_from_slice(b"..");
        root.raw[28] = self.geometry.raw[0xfc];
        if root.raw[28] > 5 {
            return Err(Error::Unsupported);
        }
        root.raw[29] = 8;
        root.limit -= 3;
        let flags = inode.flags() | INDEX_FLAG;
        inode.raw[32..36].copy_from_slice(&flags.to_le_bytes());
        self.store_index(inode, &mut root)
    }

    pub(super) fn insert_indexed(
        &mut self,
        inode: &mut Inode,
        name: &[u8],
        number: u32,
        file_type: u8,
    ) -> Result<()> {
        let mut path = self.view().index_path(inode, name)?;
        let leaf = path.leaf();
        if self.insert_in_leaf(inode, leaf, name, number, file_type)? {
            return Ok(());
        }
        let version = path.nodes[0].raw[28];
        let entries = self.view().leaf_entries(inode, leaf)?;
        let mut entries = entries
            .into_iter()
            .map(|entry| {
                Ok(HashedEntry {
                    hash: name_hash(&self.geometry, version, entry.name())?,
                    entry: LeafEntry::from(entry),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        entries.push(HashedEntry {
            hash: path.hash,
            entry: LeafEntry {
                inode: number,
                name: name.to_vec(),
                file_type,
            },
        });
        entries.sort_by(|left, right| {
            left.hash
                .cmp(&right.hash)
                .then_with(|| left.entry.name.cmp(&right.entry.name))
        });
        let total: usize = entries.iter().map(|entry| entry.entry.encoded_size()).sum();
        let mut occupied = 0;
        let split = entries
            .iter()
            .position(|entry| {
                occupied += entry.entry.encoded_size();
                occupied >= total / 2
            })
            .ok_or(Error::Corrupt)?
            + 1;
        if split == entries.len() {
            return Err(Error::Corrupt);
        }
        let boundary =
            entries[split].hash | u32::from(entries[split - 1].hash == entries[split].hash);
        let right = self.append_directory_block(inode)?;
        let mut left_entries = Vec::with_capacity(split);
        let mut right_entries = Vec::with_capacity(entries.len() - split);
        for (index, record) in entries.into_iter().enumerate() {
            if index < split {
                left_entries.push(record.entry);
            } else {
                right_entries.push(record.entry);
            }
        }
        for (logical, entries) in [(leaf, left_entries), (right, right_entries)] {
            let raw = self.encode_leaf(inode, &entries)?;
            self.store_directory_block(inode, logical, &raw)?;
        }
        let mut addition = Entry {
            hash: boundary,
            block: right,
        };
        while let Some(mut node) = path.nodes.pop() {
            node.entries.insert(node.selected + 1, addition);
            if node.entries.len() <= node.limit {
                return self.store_index(inode, &mut node);
            }
            if node.logical == 0 {
                if node.raw[30] != 0 {
                    return Err(Error::NoSpace);
                }
                let logical = self.append_directory_block(inode)?;
                let entries = core::mem::take(&mut node.entries);
                let mut interior = Node::interior(&self.geometry, logical, entries);
                self.store_index(inode, &mut interior)?;
                node.raw[30] = 1;
                node.entries.push(Entry {
                    hash: 0,
                    block: logical,
                });
                return self.store_index(inode, &mut node);
            }
            let split = node.entries.len() / 2;
            let entries = node.entries.split_off(split);
            let logical = self.append_directory_block(inode)?;
            addition = Entry {
                hash: entries[0].hash,
                block: logical,
            };
            let mut right = Node::interior(&self.geometry, logical, entries);
            self.store_index(inode, &mut node)?;
            self.store_index(inode, &mut right)?;
        }
        Err(Error::Corrupt)
    }

    pub(super) fn set_indexed_parent(
        &mut self,
        inode: &Inode,
        old_parent: u32,
        new_parent: u32,
    ) -> Result<()> {
        let mut root = self.view().index_node(inode, 0)?;
        if crate::u32_at(&root.raw, 12)? != old_parent {
            return Err(Error::Corrupt);
        }
        root.raw[12..16].copy_from_slice(&new_parent.to_le_bytes());
        self.store_index(inode, &mut root)
    }

    fn append_directory_block(&mut self, inode: &mut Inode) -> Result<u32> {
        let logical = u32::try_from(inode.size() / u64::from(self.geometry.block_size()))
            .map_err(|_| Error::NoSpace)?;
        self.ensure_file_block(inode, logical)?;
        inode.set_size(
            inode
                .size()
                .checked_add(u64::from(self.geometry.block_size()))
                .ok_or(Error::NoSpace)?,
        );
        Ok(logical)
    }

    fn store_directory_block(&mut self, inode: &Inode, logical: u32, raw: &[u8]) -> Result<()> {
        let physical = self
            .view()
            .file_block(inode, logical)?
            .ok_or(Error::Corrupt)?;
        self.check_file_block(physical)?;
        self.metadata_block(physical)?.copy_from_slice(raw);
        Ok(())
    }

    fn store_index(&mut self, inode: &Inode, node: &mut Node) -> Result<()> {
        node.encode(&self.geometry, inode)?;
        self.store_directory_block(inode, node.logical, &node.raw)
    }
}
