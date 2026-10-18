// SPDX-License-Identifier: BSD-2-Clause AND RSA-MD

//! Ext4 directory hashes, ported from lwext4's `ext4_hash.c`.
//!
//! The half-MD4 transform is derived from the RSA Data Security, Inc. MD4
//! Message-Digest Algorithm. See `LICENSE-directory-hash` for the notices.

use crate::{Error, Geometry, Result, u32_at};

pub(crate) fn name_hash(geometry: &Geometry, version: u8, name: &[u8]) -> Result<u32> {
    let version = if version <= 2 && u32_at(&geometry.raw, 0x160)? & 2 != 0 {
        version + 3
    } else {
        version
    };
    let unsigned = version >= 3;
    let mut state = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let seed = [
        u32_at(&geometry.raw, 0xec)?,
        u32_at(&geometry.raw, 0xf0)?,
        u32_at(&geometry.raw, 0xf4)?,
        u32_at(&geometry.raw, 0xf8)?,
    ];
    if seed != [0; 4] {
        state = seed;
    }
    let major = match version {
        0 | 3 => legacy(name, unsigned),
        1 | 2 | 4 | 5 => {
            let width = if version % 3 == 1 { 32 } else { 16 };
            for offset in (0..name.len()).step_by(width) {
                let words = prepare(&name[offset..], width, unsigned);
                if width == 32 {
                    half_md4(&mut state, &words);
                } else {
                    tea(&mut state, &words);
                }
            }
            state[usize::from(width == 32)]
        }
        _ => return Err(Error::Unsupported),
    } & !1;
    Ok(if major == 0xffff_fffe {
        0xffff_fffc
    } else {
        major
    })
}

fn character(byte: u8, unsigned: bool) -> u32 {
    if unsigned {
        u32::from(byte)
    } else {
        i32::from(byte as i8) as u32
    }
}

fn legacy(name: &[u8], unsigned: bool) -> u32 {
    let (mut first, mut second) = (0x12a3_fe2du32, 0x37ab_e8f9u32);
    for &byte in name {
        let mut next =
            second.wrapping_add(first ^ character(byte, unsigned).wrapping_mul(0x006d_22f5));
        if next & 0x8000_0000 != 0 {
            next = next.wrapping_sub(0x7fff_ffff);
        }
        second = first;
        first = next;
    }
    first << 1
}

fn prepare(name: &[u8], width: usize, unsigned: bool) -> [u32; 8] {
    let padding = (name.len() as u32).wrapping_mul(0x0101_0101);
    let mut words = [padding; 8];
    for (index, bytes) in name[..name.len().min(width)].chunks(4).enumerate() {
        let mut word = padding;
        for &byte in bytes {
            word = (word << 8).wrapping_add(character(byte, unsigned));
        }
        words[index] = word;
    }
    words
}

fn tea(state: &mut [u32; 4], words: &[u32; 8]) {
    let (mut x, mut y) = (state[0], state[1]);
    for round in 1u32..=16 {
        let sum = round.wrapping_mul(0x9e37_79b9);
        x = x.wrapping_add(
            (y << 4).wrapping_add(words[0]) ^ y.wrapping_add(sum) ^ (y >> 5).wrapping_add(words[1]),
        );
        y = y.wrapping_add(
            (x << 4).wrapping_add(words[2]) ^ x.wrapping_add(sum) ^ (x >> 5).wrapping_add(words[3]),
        );
    }
    state[0] = state[0].wrapping_add(x);
    state[1] = state[1].wrapping_add(y);
}

fn half_md4(state: &mut [u32; 4], words: &[u32; 8]) {
    let mut work = *state;
    let schedules = [
        ([0, 1, 2, 3, 4, 5, 6, 7], [3, 7, 11, 19], 0),
        ([1, 3, 5, 7, 0, 2, 4, 6], [3, 5, 9, 13], 0x5a82_7999),
        ([3, 7, 2, 6, 1, 5, 0, 4], [3, 9, 11, 15], 0x6ed9_eba1),
    ];
    for (round, (order, shifts, constant)) in schedules.into_iter().enumerate() {
        for (step, index) in order.into_iter().enumerate() {
            let a = (4 - step % 4) % 4;
            let (b, c, d) = (work[(a + 1) % 4], work[(a + 2) % 4], work[(a + 3) % 4]);
            let mix = match round {
                0 => (b & c) | (!b & d),
                1 => (b & c) | (b & d) | (c & d),
                _ => b ^ c ^ d,
            };
            work[a] = work[a]
                .wrapping_add(mix)
                .wrapping_add(words[index])
                .wrapping_add(constant)
                .rotate_left(shifts[step % 4]);
        }
    }
    for (value, delta) in state.iter_mut().zip(work) {
        *value = value.wrapping_add(delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_e2fsprogs_signed_unsigned_and_multiblock_vectors() {
        let mut geometry = Geometry::parse(
            *include_bytes!("../../tests/fixtures/journal-4k.super"),
            64 << 20,
        )
        .unwrap();
        for (index, value) in [0x01234567u32, 0x89abcdef, 0xfedcba98, 0x76543210]
            .into_iter()
            .enumerate()
        {
            geometry.raw[0xec + index * 4..0xf0 + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        geometry.raw[0x160..0x164].fill(0);
        let long: alloc::vec::Vec<u8> = (1..=255).collect();
        let vectors: &[(&[u8], [u32; 6])] = &[
            (
                b"name0537",
                [
                    0x0df96a9c, 0x7be3b62c, 0x1f5450ca, 0x0df96a9c, 0x7be3b62c, 0x1f5450ca,
                ],
            ),
            (
                b"A\x80\xfex",
                [
                    0x66df4842, 0x24193da2, 0x9cf7d084, 0x3f426a44, 0xe30ab4d8, 0xf6f25e6e,
                ],
            ),
            (
                &long,
                [
                    0x5645ed16, 0x8232ab4c, 0x1d6c9a44, 0xce70c1f2, 0x41dde248, 0xa2b39184,
                ],
            ),
        ];
        for &(name, hashes) in vectors {
            for (version, expected) in hashes.into_iter().enumerate() {
                assert_eq!(name_hash(&geometry, version as u8, name), Ok(expected));
            }
            geometry.raw[0x160] = 2;
            for version in 0..3 {
                assert_eq!(
                    name_hash(&geometry, version, name),
                    Ok(hashes[version as usize + 3])
                );
            }
            geometry.raw[0x160] = 0;
        }
    }
}
