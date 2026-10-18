// SPDX-License-Identifier: MPL-2.0

//! Checked attribute tables shared by inline and external storage.

use alloc::vec::Vec;

use super::Attribute;
use crate::{Error, Result, u16_at, u32_at};

pub(super) struct Packed {
    pub(super) remaining: Vec<Attribute>,
    pub(super) count: usize,
    pub(super) hash: u32,
}

pub(super) fn pack_entries(
    raw: &mut [u8],
    begin: usize,
    value_base: usize,
    attributes: Vec<Attribute>,
    external: bool,
) -> Result<Packed> {
    let mut offset = begin;
    let mut values = raw.len();
    let mut remaining = Vec::new();
    let mut count = 0;
    let mut hash = 0u32;
    let mut hashable = true;
    for attribute in attributes {
        if attribute.name.len() > 255 || attribute.name.contains(&0) {
            return Err(Error::InvalidInput);
        }
        let padded = attribute.value.len().checked_add(3).ok_or(Error::NoSpace)? & !3;
        let end = (offset + 16 + attribute.name.len()).next_multiple_of(4);
        let Some(value) = values.checked_sub(padded).filter(|value| end + 4 <= *value) else {
            remaining.push(attribute);
            continue;
        };
        raw[offset] = attribute.name.len() as u8;
        raw[offset + 1] = attribute.index;
        raw[offset + 8..offset + 12].copy_from_slice(&(attribute.value.len() as u32).to_le_bytes());
        raw[offset + 16..offset + 16 + attribute.name.len()].copy_from_slice(&attribute.name);
        if !attribute.value.is_empty() {
            let relative = u16::try_from(value.checked_sub(value_base).ok_or(Error::Corrupt)?)
                .map_err(|_| Error::NoSpace)?;
            raw[offset + 2..offset + 4].copy_from_slice(&relative.to_le_bytes());
            raw[value..value + attribute.value.len()].copy_from_slice(&attribute.value);
        }
        if external {
            let mut entry_hash = 0u32;
            for byte in &attribute.name {
                entry_hash = entry_hash.rotate_left(5) ^ u32::from(*byte);
            }
            for word in raw[value..value + padded].as_chunks::<4>().0 {
                entry_hash = entry_hash.rotate_left(16) ^ u32::from_le_bytes(*word);
            }
            raw[offset + 12..offset + 16].copy_from_slice(&entry_hash.to_le_bytes());
            hashable &= entry_hash != 0;
            hash = hash.rotate_left(16) ^ entry_hash;
        }
        count += 1;
        offset = end;
        values = value;
    }
    Ok(Packed {
        remaining,
        count,
        hash: if hashable { hash } else { 0 },
    })
}

pub(super) fn visit_entries(
    raw: &[u8],
    mut offset: usize,
    value_base: usize,
    sorted: bool,
    visit: &mut impl FnMut(u8, &[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let mut values = raw.len();
    let mut previous = None;
    loop {
        if offset.checked_add(4).is_none_or(|end| end > values) {
            return Err(Error::Corrupt);
        }
        if u32_at(raw, offset)? == 0 {
            return Ok(());
        }
        if offset + 16 > values {
            return Err(Error::Corrupt);
        }
        let length = usize::from(raw[offset]);
        let end = (offset + 16 + length).next_multiple_of(4);
        if end > values {
            return Err(Error::Corrupt);
        }
        let name = &raw[offset + 16..offset + 16 + length];
        let index = raw[offset + 1];
        let key = (index, length, name);
        if name.contains(&0) || (sorted && previous.is_some_and(|previous| previous >= key)) {
            return Err(Error::Corrupt);
        }
        previous = Some(key);
        if u32_at(raw, offset + 4)? != 0 {
            return Err(Error::Unsupported);
        }
        let size = u32_at(raw, offset + 8)? as usize;
        let value = if size == 0 {
            &[]
        } else {
            let relative = usize::from(u16_at(raw, offset + 2)?);
            let position = value_base.checked_add(relative).ok_or(Error::Corrupt)?;
            if !relative.is_multiple_of(4)
                || position < end
                || size > raw.len().saturating_sub(position)
            {
                return Err(Error::Corrupt);
            }
            values = values.min(position);
            &raw[position..position + size]
        };
        visit(index, name, value)?;
        offset = end;
    }
}
