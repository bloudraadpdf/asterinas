// SPDX-License-Identifier: MIT
// Copyright (C) 2018-2019 HUAWEI, Inc.
// Created by Gao Xiang <xiang@kernel.org>
// Modified by Huang Jianan <huangjianan@oppo.com>

//! EROFS compression indexes and bounded LZ4 decoding.
//!
//! Compact index decoding follows the MIT-licensed `erofs-utils/lib/zmap.c`
//! at revision `5fcb2f76f7b95df87f5c6d3a580e4d756ceaa1c8`.

use alloc::vec::Vec;

use io_util::ReadAt;

use super::{Erofs, Error, Inode, Result, le16, le32};

struct Index {
    kind: u8,
    offset: u64,
    block: u64,
    back: u64,
}

impl<D: ReadAt> Erofs<D> {
    pub(super) fn read_compressed(
        &mut self,
        inode: &Inode,
        offset: u64,
        output: &mut [u8],
    ) -> Result<()> {
        let header_offset = (inode.offset + inode.metadata_size)
            .checked_add(7)
            .ok_or(Error::Corrupt)?
            & !7;
        let mut header = [0; 8];
        self.read(header_offset, &mut header)?;
        let advice = le16(&header, 4)?;
        if header[6] != 0
            || header[7] != 0
            || advice & !1 != 0
            || (inode.layout == 1 && advice != 0)
        {
            return Err(Error::Unsupported);
        }
        let mut copied = 0;
        while copied < output.len() {
            let position = offset + copied as u64;
            let mut cluster = position / self.block_size;
            let mut index = self.index(inode, header_offset, advice, cluster)?;
            let mut end = (cluster + 1)
                .checked_mul(self.block_size)
                .ok_or(Error::Corrupt)?
                .min(inode.size);
            if index.kind != 2 && position % self.block_size < index.offset {
                end = cluster * self.block_size + index.offset;
                index.back = 1;
                index.kind = 2;
            }
            while index.kind == 2 {
                if index.back == 0 {
                    return Err(Error::Corrupt);
                }
                cluster = cluster.checked_sub(index.back).ok_or(Error::Corrupt)?;
                index = self.index(inode, header_offset, advice, cluster)?;
            }
            let start = cluster * self.block_size + index.offset;
            let skip = position.checked_sub(start).ok_or(Error::Corrupt)?;
            let count = end
                .checked_sub(position)
                .ok_or(Error::Corrupt)?
                .min((output.len() - copied) as u64) as usize;
            if count == 0 || skip + count as u64 > 12 * 1024 * 1024 {
                return Err(Error::Corrupt);
            }
            let physical = index
                .block
                .checked_mul(self.block_size)
                .ok_or(Error::Corrupt)?;
            let target = &mut output[copied..copied + count];
            match index.kind {
                0 => {
                    if skip + count as u64 > self.block_size {
                        return Err(Error::Corrupt);
                    }
                    self.read(physical + skip, target)?;
                }
                1 | 3 => {
                    let key = (inode.offset, cluster, physical);
                    let mut decoder = core::mem::take(&mut self.decoder);
                    let result = (|| {
                        if decoder.key != Some(key) || decoder.produced > skip as usize {
                            decoder.key = None;
                            decoder.input.resize(self.block_size as usize, 0);
                            self.read(physical, &mut decoder.input)?;
                            let padding = if self.incompatible & 1 != 0 {
                                decoder
                                    .input
                                    .iter()
                                    .position(|byte| *byte != 0)
                                    .ok_or(Error::Corrupt)?
                            } else {
                                0
                            };
                            decoder.cursor = padding;
                            decoder.produced = 0;
                            decoder.phase = Phase::Token;
                            decoder.history.resize(65536, 0);
                            decoder.key = Some(key);
                        }
                        decoder.read(skip as usize, target)
                    })();
                    if result.is_err() {
                        decoder.key = None;
                    }
                    self.decoder = decoder;
                    result?;
                }
                _ => return Err(Error::Corrupt),
            }
            copied += count;
        }
        Ok(())
    }

    fn index(&self, inode: &Inode, header: u64, advice: u16, cluster: u64) -> Result<Index> {
        if cluster >= inode.size.div_ceil(self.block_size) {
            return Err(Error::Corrupt);
        }
        if inode.layout == 1 {
            let offset = cluster
                .checked_mul(8)
                .and_then(|value| value.checked_add(header + 16))
                .ok_or(Error::Corrupt)?;
            let mut entry = [0; 8];
            self.read(offset, &mut entry)?;
            let kind = (le16(&entry, 0)? & 3) as u8;
            let result = Index {
                kind,
                offset: u64::from(le16(&entry, 2)?),
                block: u64::from(le32(&entry, 4)?),
                back: u64::from(le16(&entry, 4)?),
            };
            if kind != 2 && result.offset >= self.block_size {
                return Err(Error::Corrupt);
            }
            return Ok(result);
        }
        let base = header + 8;
        let initial = ((32 - base % 32) / 4) & 7;
        let total = inode.size.div_ceil(self.block_size);
        let compact = if advice & 1 != 0 && initial < total {
            (total - initial) / 16 * 16
        } else {
            0
        };
        let (position, shift, count) = if cluster < initial {
            (base + cluster * 4, 2, 2)
        } else if cluster - initial < compact {
            (base + initial * 4 + (cluster - initial) * 2, 1, 16)
        } else {
            (
                base + initial * 4 + compact * 2 + (cluster - initial - compact) * 4,
                2,
                2,
            )
        };
        let pack_size = count << shift;
        let mut pack = [0; 32];
        self.read(
            position / pack_size * pack_size,
            &mut pack[..pack_size as usize],
        )?;
        let item = ((position % pack_size) >> shift) as usize;
        let bits = ((pack_size - 4) * 8 / count) as usize;
        let low_bits = self.block_size.trailing_zeros().max(12);
        let decode = |item: usize| -> Result<(u8, u64)> {
            let bit = item * bits;
            let value = le32(&pack, bit / 8)? >> (bit % 8);
            Ok((
                ((value >> low_bits) & 3) as u8,
                u64::from(value & ((1 << low_bits) - 1)),
            ))
        };
        let (kind, low) = decode(item)?;
        if kind == 2 {
            let back = if item + 1 == count as usize {
                let (previous_kind, previous_low) = decode(item - 1)?;
                if previous_kind == 2 {
                    previous_low + 1
                } else {
                    1
                }
            } else {
                low
            };
            if back & (1 << 11) != 0 {
                return Err(Error::Corrupt);
            }
            return Ok(Index {
                kind,
                offset: 0,
                block: 0,
                back,
            });
        }
        if low >= self.block_size {
            return Err(Error::Corrupt);
        }
        let mut blocks = 1;
        let mut previous = item as i32 - 1;
        while previous >= 0 {
            let (kind, delta) = decode(previous as usize)?;
            if kind == 2 {
                previous -= i32::try_from(delta).map_err(|_| Error::Corrupt)?;
            }
            if previous >= 0 {
                blocks += 1;
            }
            previous -= 1;
        }
        let block = u64::from(le32(&pack, pack_size as usize - 4)?) + blocks;
        Ok(Index {
            kind,
            offset: low,
            block,
            back: 0,
        })
    }
}

fn extended_length(input: &[u8], cursor: &mut usize, initial: usize) -> Result<usize> {
    let mut length = initial;
    if initial == 15 {
        loop {
            let byte = *input.get(*cursor).ok_or(Error::Corrupt)?;
            *cursor += 1;
            length = length
                .checked_add(usize::from(byte))
                .ok_or(Error::Corrupt)?;
            if byte != 255 {
                break;
            }
        }
    }
    Ok(length)
}

#[derive(Default)]
pub(super) struct Decoder {
    key: Option<(u64, u64, u64)>,
    input: Vec<u8>,
    history: Vec<u8>,
    cursor: usize,
    produced: usize,
    phase: Phase,
    match_code: usize,
}

#[derive(Default)]
enum Phase {
    #[default]
    Token,
    Literals(usize),
    Offset,
    Match {
        distance: usize,
        remaining: usize,
    },
}

impl Decoder {
    fn read(&mut self, skip: usize, output: &mut [u8]) -> Result<()> {
        let end = skip.checked_add(output.len()).ok_or(Error::Corrupt)?;
        while self.produced < end {
            match self.phase {
                Phase::Token => {
                    let token = *self.input.get(self.cursor).ok_or(Error::Corrupt)?;
                    self.cursor += 1;
                    self.match_code = usize::from(token & 15);
                    let count =
                        extended_length(&self.input, &mut self.cursor, usize::from(token >> 4))?;
                    if self
                        .cursor
                        .checked_add(count)
                        .is_none_or(|end| end > self.input.len())
                    {
                        return Err(Error::Corrupt);
                    }
                    self.phase = Phase::Literals(count);
                }
                Phase::Literals(remaining) => {
                    let count = remaining.min(end - self.produced);
                    for _ in 0..count {
                        let byte = self.input[self.cursor];
                        self.cursor += 1;
                        self.emit(byte, skip, output);
                    }
                    self.phase = if remaining == count {
                        Phase::Offset
                    } else {
                        Phase::Literals(remaining - count)
                    };
                }
                Phase::Offset => {
                    let distance = usize::from(le16(&self.input, self.cursor)?);
                    self.cursor += 2;
                    if distance == 0 || distance > self.produced || distance > self.history.len() {
                        return Err(Error::Corrupt);
                    }
                    let remaining =
                        extended_length(&self.input, &mut self.cursor, self.match_code)?
                            .checked_add(4)
                            .ok_or(Error::Corrupt)?;
                    self.phase = Phase::Match {
                        distance,
                        remaining,
                    };
                }
                Phase::Match {
                    distance,
                    remaining,
                } => {
                    let count = remaining.min(end - self.produced);
                    for _ in 0..count {
                        let byte = self.history[(self.produced - distance) % self.history.len()];
                        self.emit(byte, skip, output);
                    }
                    self.phase = if remaining == count {
                        Phase::Token
                    } else {
                        Phase::Match {
                            distance,
                            remaining: remaining - count,
                        }
                    };
                }
            }
        }
        Ok(())
    }

    fn emit(&mut self, byte: u8, skip: usize, output: &mut [u8]) {
        let index = self.produced % self.history.len();
        self.history[index] = byte;
        if self.produced >= skip {
            output[self.produced - skip] = byte;
        }
        self.produced += 1;
    }
}
