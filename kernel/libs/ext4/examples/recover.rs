// SPDX-License-Identifier: MPL-2.0

//! Replays a writable image in place for Linux interoperability checks.

use aster_ext4::{Ext4, MountLimits};

mod common;

fn main() {
    let image = std::env::args().nth(1).expect("usage: recover IMAGE");
    let limits = MountLimits {
        journal_blocks: 4096,
        transactions: 1024,
        revocations: 16384,
        mapping_entries: 4096,
        orphan_inodes: 8192,
        metadata_blocks: 32,
        data_blocks: 4,
        allocation_groups: 4096,
        directory_depth: 1024,
    };
    let fs = Ext4::recover(common::Disk::open(&image, true).unwrap(), limits).unwrap();
    let root = fs.inode(2).unwrap();
    let entries = fs
        .read_dir(&root)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    println!(
        "EXT4_RECOVERY_PASSED image={image} root_entries={} free_blocks={} free_inodes={}",
        entries.len(),
        fs.geometry().free_blocks(),
        fs.geometry().free_inodes()
    );
}
