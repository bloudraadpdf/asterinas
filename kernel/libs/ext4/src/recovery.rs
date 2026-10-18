// SPDX-License-Identifier: MPL-2.0

//! Bounded JBD2 replay before volume admission.

use alloc::vec::Vec;

use io_util::{ReadAt, WriteAt, checksum};

use super::{
    Error, Ext4, Geometry, Journal, JournalChecksum, JournalTag, MountLimits, Result, journal,
};

#[derive(Debug)]
struct Run {
    logical: u32,
    physical: u64,
    blocks: u32,
}

pub(super) struct JournalMap {
    runs: Vec<Run>,
    physical_runs: Vec<usize>,
    nodes: Vec<u64>,
    inode_offset: u64,
}

#[derive(Debug)]
struct Transaction {
    start: u32,
    sequence: u32,
    blocks: u32,
}

#[derive(Debug)]
struct Revocation {
    block: u64,
    transaction: usize,
}

enum Record<'a> {
    Data(&'a JournalTag, &'a mut [u8]),
    Revocation(u64),
}

impl<D: WriteAt> Ext4<D> {
    /// Replays committed journal records and admits the resulting clean volume.
    ///
    /// The caller must provide exclusive access to the device throughout its use.
    /// Failed recovery never returns an admitted volume or clears recovery flags.
    pub fn recover(device: D, limits: MountLimits) -> Result<Self> {
        let mut raw = [0; 1024];
        device.read_exact_at(1024, &mut raw)?;
        let geometry = Geometry::parse(raw, device.len())?;
        let mut fs = Self { device, geometry };
        if fs.geometry.has_errors() {
            return Err(Error::Corrupt);
        }
        if !fs.geometry.is_clean() && fs.geometry.journal_inode() == 0 {
            return Err(Error::Unsupported);
        }
        if !fs.geometry.needs_recovery()
            && !fs.geometry.orphan_cleanup_required()
            && fs.geometry.last_orphan() == 0
        {
            return Ok(fs);
        }
        if fs.device.is_read_only() {
            return Err(Error::ReadOnly);
        }
        if fs.geometry.needs_recovery() {
            let journal = fs.journal()?.ok_or(Error::Corrupt)?;
            fs.replay(&journal, limits)?;
            fs.device.read_exact_at(1024, &mut raw)?;
            fs.geometry = Geometry::parse(raw, fs.device.len())?;
            fs.inode(fs.geometry.journal_inode())?;
        }
        let entries = fs.orphans(limits.orphan_inodes)?;
        for entry in &entries {
            let inode = fs.inode(entry.number)?;
            fs.check_reclaimable(&inode)?;
        }
        if !entries.is_empty() {
            let mut session = crate::WritableExt4::resume(fs, limits)?;
            for entry in entries {
                if session.reader()?.inode(entry.number)?.links() == 0 {
                    session.reap(entry.number)?;
                } else {
                    session.finish_truncate(entry.number)?;
                }
            }
            return session.close();
        }
        let raw = fs.geometry.clean_superblock();
        fs.device.write_all_at(1024, &raw)?;
        fs.device.flush()?;
        fs.geometry = Geometry::parse(raw, fs.device.len())?;
        Ok(fs)
    }

    fn replay(&self, journal: &Journal, limits: MountLimits) -> Result<()> {
        if journal.blocks() > limits.journal_blocks {
            return Err(Error::ResourceLimit);
        }
        let map = self.journal_map(journal, limits.mapping_entries)?;
        let read_fn = |logical| self.read_block(map.physical(logical)?);
        let transactions = journal.scan(&read_fn, limits.transactions)?;
        let mut revocations = Vec::new();
        for (rank, transaction) in transactions.iter().enumerate() {
            journal.visit(transaction, &read_fn, &mut |record| {
                match record {
                    Record::Data(tag, raw) => map.check_target(self, journal, tag.block(), raw)?,
                    Record::Revocation(block) => {
                        self.geometry.block_offset(block)?;
                        push_bounded(
                            &mut revocations,
                            Revocation {
                                block,
                                transaction: rank,
                            },
                            limits.revocations,
                        )?;
                    }
                }
                Ok(())
            })?;
        }
        revocations
            .sort_unstable_by_key(|entry| (entry.block, core::cmp::Reverse(entry.transaction)));
        revocations.dedup_by_key(|entry| entry.block);
        for (rank, transaction) in transactions.iter().enumerate() {
            journal.visit(transaction, &read_fn, &mut |record| {
                let Record::Data(tag, raw) = record else {
                    return Ok(());
                };
                if let Ok(index) =
                    revocations.binary_search_by_key(&tag.block(), |entry| entry.block)
                    && rank <= revocations[index].transaction
                {
                    return Ok(());
                }
                let offset = self.geometry.block_offset(tag.block())?;
                if tag.block() == 1024 / u64::from(self.geometry.block_size()) {
                    let within = 1024 % self.geometry.block_size() as usize;
                    let geometry =
                        Geometry::parse(super::bytes_at(raw, within)?, self.device.len())?;
                    raw[within..within + 1024].copy_from_slice(&geometry.recovery_superblock());
                }
                self.device.write_all_at(offset, raw)?;
                Ok(())
            })?;
        }
        self.device.flush()?;
        let sequence = transactions
            .last()
            .map_or(journal.sequence(), |transaction| {
                transaction.sequence.wrapping_add(1)
            });
        self.device.write_all_at(
            self.geometry.block_offset(map.physical(0)?)?,
            &journal.clean_superblock(sequence),
        )?;
        self.device.flush()?;
        Ok(())
    }
}

impl<D: ReadAt> Ext4<D> {
    pub(super) fn journal_map(&self, journal: &Journal, limit: usize) -> Result<JournalMap> {
        let inode = &journal.inode;
        if inode.mode() & 0xf000 != 0x8000 {
            return Err(Error::Corrupt);
        }
        let group = self.group((inode.number() - 1) / self.geometry.inodes_per_group())?;
        let inode_offset = self.geometry.block_offset(group.inode_table())?
            + u64::from((inode.number() - 1) % self.geometry.inodes_per_group())
                * u64::from(self.geometry.inode_size());
        let mut map = JournalMap {
            runs: Vec::new(),
            physical_runs: Vec::new(),
            nodes: Vec::new(),
            inode_offset,
        };
        for logical in 0..journal.blocks() {
            let physical = self
                .file_block_with_nodes(inode, logical, &mut |node| match map
                    .nodes
                    .binary_search(&node)
                {
                    Ok(_) => Ok(()),
                    Err(index) => {
                        if map.nodes.len() + map.runs.len() >= limit {
                            return Err(Error::ResourceLimit);
                        }
                        map.nodes.try_reserve(1).map_err(|_| Error::ResourceLimit)?;
                        map.nodes.insert(index, node);
                        Ok(())
                    }
                })?
                .ok_or(Error::Corrupt)?;
            if let Some(run) = map.runs.last_mut()
                && run.physical + u64::from(run.blocks) == physical
            {
                run.blocks += 1;
                continue;
            }
            let remaining = limit
                .checked_sub(map.nodes.len())
                .ok_or(Error::ResourceLimit)?;
            push_bounded(
                &mut map.runs,
                Run {
                    logical,
                    physical,
                    blocks: 1,
                },
                remaining,
            )?;
        }
        map.physical_runs = (0..map.runs.len()).collect();
        map.physical_runs
            .sort_unstable_by_key(|index| map.runs[*index].physical);
        let mut end = 0;
        for index in &map.physical_runs {
            let run = &map.runs[*index];
            if run.physical < end {
                return Err(Error::Corrupt);
            }
            end = run.physical + u64::from(run.blocks);
            let node = map.nodes.partition_point(|node| *node < run.physical);
            if map.nodes.get(node).is_some_and(|node| *node < end) {
                return Err(Error::Corrupt);
            }
        }
        Ok(map)
    }
}

impl JournalMap {
    pub(super) fn reserved_ranges(&self) -> impl Iterator<Item = core::ops::Range<u64>> + '_ {
        self.runs
            .iter()
            .map(|run| run.physical..run.physical + u64::from(run.blocks))
            .chain(self.nodes.iter().map(|node| *node..*node + 1))
    }
    pub(super) fn physical(&self, logical: u32) -> Result<u64> {
        let index = self
            .runs
            .partition_point(|run| run.logical <= logical)
            .checked_sub(1)
            .ok_or(Error::Corrupt)?;
        let run = &self.runs[index];
        let within = logical - run.logical;
        if within >= run.blocks {
            return Err(Error::Corrupt);
        }
        Ok(run.physical + u64::from(within))
    }

    pub(super) fn check_target<D: ReadAt>(
        &self,
        fs: &Ext4<D>,
        journal: &Journal,
        block: u64,
        raw: &[u8],
    ) -> Result<()> {
        let offset = fs.geometry.block_offset(block)?;
        let index = self
            .physical_runs
            .partition_point(|index| self.runs[*index].physical <= block);
        if index > 0 {
            let run = &self.runs[self.physical_runs[index - 1]];
            if block - run.physical < u64::from(run.blocks) {
                return Err(Error::Corrupt);
            }
        }
        if self.nodes.binary_search(&block).is_ok() {
            return Err(Error::Corrupt);
        }
        if offset <= self.inode_offset && self.inode_offset < offset + raw.len() as u64 {
            let within = (self.inode_offset - offset) as usize;
            if raw.get(within..within + journal.inode.raw.len())
                != Some(journal.inode.raw.as_slice())
            {
                return Err(Error::Corrupt);
            }
        }
        let super_block = 1024 / u64::from(fs.geometry.block_size());
        if block == super_block {
            let within = 1024 % fs.geometry.block_size() as usize;
            let super_raw = super::bytes_at(raw, within)?;
            let geometry = Geometry::parse(super_raw, fs.device.len())?;
            if geometry.blocks() != fs.geometry.blocks()
                || geometry.block_size() != fs.geometry.block_size()
                || geometry.inodes() != fs.geometry.inodes()
                || geometry.inode_size() != fs.geometry.inode_size()
                || geometry.groups() != fs.geometry.groups()
                || geometry.descriptor_size() != fs.geometry.descriptor_size()
                || geometry.blocks_per_group() != fs.geometry.blocks_per_group()
                || geometry.inodes_per_group() != fs.geometry.inodes_per_group()
                || geometry.checksum_seed() != fs.geometry.checksum_seed()
                || geometry.journal_inode() != fs.geometry.journal_inode()
            {
                return Err(Error::Corrupt);
            }
        }
        Ok(())
    }
}

impl Journal {
    fn scan(
        &self,
        read_fn: &impl Fn(u32) -> Result<Vec<u8>>,
        limit: usize,
    ) -> Result<Vec<Transaction>> {
        let mut transactions = Vec::new();
        if self.start() == 0 {
            return Ok(transactions);
        }
        let mut cursor = self.start();
        let mut remaining = self.blocks() - self.first_block();
        let mut sequence = self.sequence();
        while remaining > 0 {
            let transaction = match self.walk(cursor, sequence, remaining, read_fn, &mut |_| Ok(()))
            {
                Ok(Some(blocks)) => Transaction {
                    start: cursor,
                    sequence,
                    blocks,
                },
                Ok(None) => break,
                Err(Error::Corrupt) => {
                    if self.has_commit(cursor, sequence, remaining, read_fn)? {
                        return Err(Error::Corrupt);
                    }
                    break;
                }
                Err(error) => return Err(error),
            };
            remaining -= transaction.blocks;
            for _ in 0..transaction.blocks {
                cursor = self.next_block(cursor)?;
            }
            sequence = sequence.wrapping_add(1);
            push_bounded(&mut transactions, transaction, limit)?;
        }
        Ok(transactions)
    }

    fn visit(
        &self,
        transaction: &Transaction,
        read_fn: &impl Fn(u32) -> Result<Vec<u8>>,
        visit_fn: &mut impl FnMut(Record<'_>) -> Result<()>,
    ) -> Result<()> {
        if self.walk(
            transaction.start,
            transaction.sequence,
            transaction.blocks,
            read_fn,
            visit_fn,
        )? != Some(transaction.blocks)
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn walk(
        &self,
        start: u32,
        sequence: u32,
        budget: u32,
        read_fn: &impl Fn(u32) -> Result<Vec<u8>>,
        visit_fn: &mut impl FnMut(Record<'_>) -> Result<()>,
    ) -> Result<Option<u32>> {
        let mut cursor = start;
        let mut consumed = 0;
        let mut crc = !0;
        let mut bad_data = false;
        while consumed < budget {
            let raw = read_fn(cursor)?;
            let (kind, record_sequence) = match journal::record_header(&raw) {
                Ok(header) => header,
                Err(_) => return Err(Error::Corrupt),
            };
            if record_sequence != sequence {
                return Err(Error::Corrupt);
            }
            consumed += 1;
            cursor = self.next_block(cursor)?;
            match kind {
                1 => {
                    let tags = self.descriptor(&raw, sequence)?;
                    if self.checksum_mode() == JournalChecksum::TransactionCrc32 {
                        crc = checksum::crc32_be(crc, &raw);
                    }
                    for tag in tags {
                        if consumed >= budget {
                            return Ok(None);
                        }
                        let mut raw = read_fn(cursor)?;
                        consumed += 1;
                        cursor = self.next_block(cursor)?;
                        if self.checksum_mode() == JournalChecksum::TransactionCrc32 {
                            crc = checksum::crc32_be(crc, &raw);
                        }
                        bad_data |= self.check_data(&tag, sequence, &raw).is_err();
                        if tag.is_escaped() && raw.get(..4) != Some(&[0; 4]) {
                            bad_data = true;
                        }
                        if tag.is_escaped() {
                            raw[..4].copy_from_slice(&0xc03b_3998u32.to_be_bytes());
                        }
                        visit_fn(Record::Data(&tag, &mut raw))?;
                    }
                }
                5 => {
                    for block in self.revocations(&raw, sequence)? {
                        visit_fn(Record::Revocation(block))?;
                    }
                }
                2 => {
                    if self.check_commit(&raw, sequence, crc).is_err() {
                        return Ok(None);
                    }
                    if bad_data {
                        return Err(Error::Corrupt);
                    }
                    return Ok(Some(consumed));
                }
                _ => return Err(Error::Corrupt),
            }
        }
        Ok(None)
    }

    fn has_commit(
        &self,
        mut cursor: u32,
        sequence: u32,
        remaining: u32,
        read_fn: &impl Fn(u32) -> Result<Vec<u8>>,
    ) -> Result<bool> {
        for _ in 0..remaining {
            let raw = read_fn(cursor)?;
            if journal::record_header(&raw) == Ok((2, sequence)) {
                match self.checksum_mode() {
                    JournalChecksum::None | JournalChecksum::TransactionCrc32 => return Ok(true),
                    _ if self.check_commit(&raw, sequence, 0).is_ok() => return Ok(true),
                    _ => (),
                }
            }
            cursor = self.next_block(cursor)?;
        }
        Ok(false)
    }
}

fn push_bounded<T>(values: &mut Vec<T>, value: T, limit: usize) -> Result<()> {
    if values.len() >= limit {
        return Err(Error::ResourceLimit);
    }
    if values.len() == values.capacity() {
        values
            .try_reserve_exact((limit - values.len()).min(64))
            .map_err(|_| Error::ResourceLimit)?;
    }
    values.push(value);
    Ok(())
}
