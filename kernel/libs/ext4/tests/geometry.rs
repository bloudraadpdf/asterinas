// SPDX-License-Identifier: MPL-2.0

use aster_ext4::{Error, Geometry};
use io_util::checksum;

const SUPERBLOCKS: [[u8; 1024]; 3] = [
    *include_bytes!("fixtures/job-default.super"),
    *include_bytes!("fixtures/job-4k.super"),
    *include_bytes!("fixtures/journal-4k.super"),
];

fn checksum_superblock(raw: &mut [u8; 1024]) {
    let checksum = checksum::crc32c(!0, &raw[..1020]);
    raw[1020..].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn parses_product_template_superblocks() {
    for (raw, size) in SUPERBLOCKS.into_iter().zip([64 << 20, 512 << 20, 64 << 20]) {
        let geometry = Geometry::parse(raw, size).unwrap();
        assert_eq!(geometry.block_size(), 4096);
        assert_eq!(geometry.descriptor_size(), 64);
        assert!(geometry.has_metadata_checksums());
        assert_eq!(geometry.blocks() * u64::from(geometry.block_size()), size);
    }
    let journalled = Geometry::parse(SUPERBLOCKS[2], 64 << 20).unwrap();
    assert_eq!(journalled.journal_inode(), 8);
    assert!(journalled.has_orphan_file());
    assert!(journalled.orphan_file_inode() > 8);
    let legacy = Geometry::parse(*include_bytes!("fixtures/legacy-1k.super"), 64 << 20).unwrap();
    assert_eq!(legacy.block_size(), 1024);
    assert_eq!(legacy.groups(), 8);
}

#[test]
fn rejects_invalid_geometry_even_with_valid_checksum() {
    for (offset, count) in [(0x04, 4), (0x20, 4), (0x28, 4), (0x58, 2), (0xfe, 2)] {
        let mut raw = SUPERBLOCKS[0];
        raw[offset..offset + count].fill(0);
        checksum_superblock(&mut raw);
        assert_eq!(Geometry::parse(raw, 64 << 20).unwrap_err(), Error::Corrupt);
    }
}

#[test]
fn rejects_corrupt_checksums_and_unknown_features() {
    let mut raw = SUPERBLOCKS[0];
    raw[0x270] ^= 1;
    assert_eq!(Geometry::parse(raw, 64 << 20).unwrap_err(), Error::Corrupt);
    let mut raw = SUPERBLOCKS[0];
    raw[0x63] |= 0x80;
    checksum_superblock(&mut raw);
    assert_eq!(
        Geometry::parse(raw, 64 << 20).unwrap_err(),
        Error::Unsupported
    );
    assert_eq!(
        Geometry::parse(SUPERBLOCKS[0], (64 << 20) - 1).unwrap_err(),
        Error::Corrupt
    );
}

#[test]
fn checksum_seed_does_not_depend_on_current_uuid() {
    let original = Geometry::parse(SUPERBLOCKS[0], 64 << 20).unwrap();
    let mut raw = SUPERBLOCKS[0];
    raw[0x68..0x78].fill(0x57);
    checksum_superblock(&mut raw);
    let changed = Geometry::parse(raw, 64 << 20).unwrap();
    assert_ne!(original.uuid(), changed.uuid());
    assert_eq!(original.checksum_seed(), changed.checksum_seed());
}

#[test]
fn rejects_unaligned_or_oversized_group_descriptors() {
    for size in [0u16, 32, 96, 2048, 4096] {
        let mut raw = SUPERBLOCKS[0];
        raw[0xfe..0x100].copy_from_slice(&size.to_le_bytes());
        checksum_superblock(&mut raw);
        assert_eq!(Geometry::parse(raw, 64 << 20).unwrap_err(), Error::Corrupt);
    }
    let mut raw = SUPERBLOCKS[0];
    raw[0xfe..0x100].copy_from_slice(&128u16.to_le_bytes());
    checksum_superblock(&mut raw);
    assert_eq!(
        Geometry::parse(raw, 64 << 20).unwrap().descriptor_size(),
        128
    );
}
