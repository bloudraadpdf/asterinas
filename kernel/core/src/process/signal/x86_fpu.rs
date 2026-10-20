// SPDX-License-Identifier: MPL-2.0

use ostd::{
    arch::cpu::context::{FpuContext, FpuStateFormat},
    mm::VmIo,
};

use crate::prelude::*;

const LEGACY_SIZE: usize = 512;
const HEADER_SIZE: usize = 64;
const MAGIC1: u32 = 0x4650_5853;
const MAGIC2: u32 = 0x4650_5845;

#[repr(C)]
#[derive(Clone, Copy, Default, Pod)]
struct XstateInfo {
    magic: u32,
    extended_size: u32,
    features: u64,
    size: u32,
    _reserved: [u32; 7],
}

pub(super) struct SignalFpuImage {
    pub(super) bytes: Vec<u8>,
    pub(super) flags: u64,
}

pub(super) fn encode(context: &FpuContext) -> Result<SignalFpuImage> {
    let raw = context.as_bytes();
    let features = FpuContext::xsave_features().filter(|_| raw.len() >= LEGACY_SIZE + HEADER_SIZE);
    let trailing = if features.is_some() {
        size_of::<u32>()
    } else {
        0
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(raw.len() + trailing)
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    bytes.extend_from_slice(raw);
    bytes[464..LEGACY_SIZE].fill(0);
    let flags = if let Some(features) = features {
        let size = u32::try_from(raw.len()).map_err(|_| Error::new(Errno::EOVERFLOW))?;
        let info = XstateInfo {
            magic: MAGIC1,
            extended_size: size + 4,
            features,
            size,
            ..Default::default()
        };
        bytes[464..LEGACY_SIZE].copy_from_slice(info.as_bytes());
        let saved = u64::from_ne_bytes(core::array::from_fn(|index| bytes[LEGACY_SIZE + index]));
        bytes[LEGACY_SIZE..LEGACY_SIZE + 8]
            .copy_from_slice(&((saved & features) | 3).to_ne_bytes());
        bytes.extend_from_slice(&MAGIC2.to_ne_bytes());
        1
    } else {
        0
    };
    Ok(SignalFpuImage { bytes, flags })
}

fn legacy(bytes: &[u8]) -> Result<FpuContext> {
    FpuContext::from_bytes(bytes, FpuStateFormat::Legacy).map_err(|_| Error::new(Errno::EINVAL))
}

pub(crate) fn decode(address: Vaddr, user_space: &CurrentUserSpace) -> Result<FpuContext> {
    if address == 0 {
        return Ok(FpuContext::new());
    }
    if !address.is_multiple_of(16) {
        return_errno!(Errno::EINVAL);
    }
    let mut prefix = [0; LEGACY_SIZE + HEADER_SIZE];
    user_space.read_bytes(address, &mut prefix[..LEGACY_SIZE])?;
    let info = XstateInfo::read_from_bytes(&prefix[464..LEGACY_SIZE])
        .map_err(|_| Error::new(Errno::EINVAL))?;
    let size = info.size as usize;
    if FpuContext::xsave_features().is_none()
        || info.magic != MAGIC1
        || size < prefix.len()
        || size > FpuContext::max_image_size()
        || info.size > info.extended_size
    {
        return legacy(&prefix[..LEGACY_SIZE]);
    }
    let tail = address
        .checked_add(size)
        .ok_or_else(|| Error::new(Errno::EFAULT))?;
    if user_space.read_val::<u32>(tail)? != MAGIC2 {
        return legacy(&prefix[..LEGACY_SIZE]);
    }
    if !address.is_multiple_of(64) {
        return_errno!(Errno::EINVAL);
    }
    let header = address
        .checked_add(LEGACY_SIZE)
        .ok_or_else(|| Error::new(Errno::EFAULT))?;
    user_space.read_bytes(header, &mut prefix[LEGACY_SIZE..])?;
    let saved = u64::from_ne_bytes(core::array::from_fn(|index| prefix[LEGACY_SIZE + index]));
    let needed = FpuContext::xsave_image_size(saved & info.features)
        .map_err(|_| Error::new(Errno::EINVAL))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(needed)
        .map_err(|_| Error::new(Errno::ENOMEM))?;
    bytes.extend_from_slice(&prefix);
    bytes.resize(needed, 0);
    if needed > prefix.len() {
        let extended = address
            .checked_add(prefix.len())
            .ok_or_else(|| Error::new(Errno::EFAULT))?;
        user_space.read_bytes(extended, &mut bytes[prefix.len()..])?;
    }
    FpuContext::from_bytes(
        &bytes,
        FpuStateFormat::Xsave {
            features: info.features,
        },
    )
    .map_err(|_| Error::new(Errno::EINVAL))
}
