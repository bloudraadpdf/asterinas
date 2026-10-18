// SPDX-License-Identifier: MPL-2.0

//! Ordered JBD2 commits with a checkpoint before journal reuse.

use alloc::{collections::BTreeMap, vec, vec::Vec};

use io_util::{WriteAt, checksum};

use super::{JOURNAL_MAGIC, Journal, JournalChecksum};
use crate::{Error, Geometry, Result, recovery::JournalMap};

impl Journal {
    pub(crate) fn commit(
        &mut self,
        device: &impl WriteAt,
        geometry: &Geometry,
        map: &JournalMap,
        metadata: &BTreeMap<u64, Vec<u8>>,
    ) -> Result<()> {
        if metadata.is_empty() {
            return Ok(());
        }
        let tail_size = if self.has_block_checksums() { 4 } else { 0 };
        let tag_size = self.tag_size();
        let capacity = (self.block_size as usize - 12 - 16 - tail_size) / tag_size;
        let journal_blocks = metadata
            .len()
            .checked_add(metadata.len().div_ceil(capacity))
            .and_then(|count| count.checked_add(1))
            .ok_or(Error::ResourceLimit)?;
        if journal_blocks >= (self.blocks - self.first) as usize {
            return Err(Error::ResourceLimit);
        }
        for (block, raw) in metadata {
            if raw.len() != self.block_size as usize
                || (!self.has_64bit && *block > u64::from(u32::MAX))
            {
                return Err(Error::Corrupt);
            }
        }

        let active = self.superblock(self.first, self.sequence);
        device.write_all_at(geometry.block_offset(map.physical(0)?)?, &active)?;
        device.flush()?;
        let mut cursor = self.first;
        let mut transaction_crc = !0;
        let mut entries = metadata.iter();
        loop {
            let batch: Vec<_> = entries.by_ref().take(capacity).collect();
            if batch.is_empty() {
                break;
            }
            let mut descriptor = self.record(1);
            let mut offset = 12;
            for (index, (block, data)) in batch.iter().enumerate() {
                let escaped = data[..4] == JOURNAL_MAGIC.to_be_bytes();
                let mut flags = u32::from(escaped);
                if index > 0 {
                    flags |= 2;
                }
                if index + 1 == batch.len() {
                    flags |= 8;
                }
                descriptor[offset..offset + 4].copy_from_slice(&(**block as u32).to_be_bytes());
                let crc = if self.has_block_checksums() {
                    self.data_checksum(data, escaped)
                } else {
                    0
                };
                if self.checksum == JournalChecksum::BlockCrc32c32 {
                    descriptor[offset + 4..offset + 8].copy_from_slice(&flags.to_be_bytes());
                    descriptor[offset + 8..offset + 12]
                        .copy_from_slice(&((**block >> 32) as u32).to_be_bytes());
                    descriptor[offset + 12..offset + 16].copy_from_slice(&crc.to_be_bytes());
                } else {
                    descriptor[offset + 4..offset + 6].copy_from_slice(&(crc as u16).to_be_bytes());
                    descriptor[offset + 6..offset + 8]
                        .copy_from_slice(&(flags as u16).to_be_bytes());
                    if self.has_64bit {
                        descriptor[offset + 8..offset + 12]
                            .copy_from_slice(&((**block >> 32) as u32).to_be_bytes());
                    }
                }
                offset += tag_size;
                if index == 0 {
                    descriptor[offset..offset + 16].copy_from_slice(&self.raw_super[48..64]);
                    offset += 16;
                }
            }
            if self.has_block_checksums() {
                let crc = checksum::crc32c(self.checksum_seed, &descriptor);
                descriptor[self.block_size as usize - 4..].copy_from_slice(&crc.to_be_bytes());
            }
            if self.checksum == JournalChecksum::TransactionCrc32 {
                transaction_crc = checksum::crc32_be(transaction_crc, &descriptor);
            }
            self.write_log_block(device, geometry, map, &mut cursor, &descriptor)?;
            for (_, data) in batch {
                if data[..4] == JOURNAL_MAGIC.to_be_bytes() {
                    let mut wire = data.clone();
                    wire[..4].fill(0);
                    if self.checksum == JournalChecksum::TransactionCrc32 {
                        transaction_crc = checksum::crc32_be(transaction_crc, &wire);
                    }
                    self.write_log_block(device, geometry, map, &mut cursor, &wire)?;
                } else {
                    if self.checksum == JournalChecksum::TransactionCrc32 {
                        transaction_crc = checksum::crc32_be(transaction_crc, data);
                    }
                    self.write_log_block(device, geometry, map, &mut cursor, data)?;
                }
            }
        }
        device.flush()?;
        let mut commit = self.record(2);
        match self.checksum {
            JournalChecksum::None => (),
            JournalChecksum::TransactionCrc32 => {
                commit[12] = 1;
                commit[13] = 4;
                commit[16..20].copy_from_slice(&transaction_crc.to_be_bytes());
            }
            _ => {
                let crc = checksum::crc32c(self.checksum_seed, &commit);
                commit[16..20].copy_from_slice(&crc.to_be_bytes());
            }
        }
        self.write_log_block(device, geometry, map, &mut cursor, &commit)?;
        device.flush()?;
        for (block, raw) in metadata {
            device.write_all_at(geometry.block_offset(*block)?, raw)?;
        }
        device.flush()?;
        let sequence = self.sequence.wrapping_add(1);
        let raw = self.clean_superblock(sequence);
        device.write_all_at(geometry.block_offset(map.physical(0)?)?, &raw)?;
        device.flush()?;
        self.raw_super = raw;
        self.start = 0;
        self.sequence = sequence;
        Ok(())
    }

    fn has_block_checksums(&self) -> bool {
        matches!(
            self.checksum,
            JournalChecksum::BlockCrc32c16 | JournalChecksum::BlockCrc32c32
        )
    }

    fn record(&self, kind: u32) -> Vec<u8> {
        let mut raw = vec![0; self.block_size as usize];
        raw[..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
        raw[4..8].copy_from_slice(&kind.to_be_bytes());
        raw[8..12].copy_from_slice(&self.sequence.to_be_bytes());
        raw
    }

    fn data_checksum(&self, raw: &[u8], escaped: bool) -> u32 {
        let crc = checksum::crc32c(self.checksum_seed, &self.sequence.to_be_bytes());
        if escaped {
            checksum::crc32c(checksum::crc32c(crc, &[0; 4]), &raw[4..])
        } else {
            checksum::crc32c(crc, raw)
        }
    }

    fn write_log_block(
        &self,
        device: &impl WriteAt,
        geometry: &Geometry,
        map: &JournalMap,
        cursor: &mut u32,
        raw: &[u8],
    ) -> Result<()> {
        device.write_all_at(geometry.block_offset(map.physical(*cursor)?)?, raw)?;
        *cursor = self.next_block(*cursor)?;
        Ok(())
    }
}
