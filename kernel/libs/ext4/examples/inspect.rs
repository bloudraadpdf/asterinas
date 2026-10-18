// SPDX-License-Identifier: MPL-2.0

use aster_ext4::Ext4;

mod common;

fn main() {
    let image = std::env::args().nth(1).expect("usage: inspect IMAGE");
    let fs = Ext4::open(common::Disk::open(&image, false).unwrap()).unwrap();
    let geometry = fs.geometry();
    let root = fs.inode(2).unwrap();
    assert_eq!(root.mode() & 0xf000, 0x4000);
    let mut bytes = [0; 4096];
    assert!(fs.read_at(&root, 0, &mut bytes).unwrap() > 0);
    assert!(fs.file_block(&root, 0).unwrap().is_some());
    for group in 0..geometry.groups() {
        fs.group(group).unwrap();
    }
    if geometry.journal_inode() != 0 {
        let log = fs.journal().unwrap().unwrap();
        assert_eq!(log.start(), 0);
        assert!(log.blocks() > log.first_block());
        let journal = fs.inode(geometry.journal_inode()).unwrap();
        assert_eq!(journal.mode() & 0xf000, 0x8000);
        assert!(fs.file_block(&journal, 0).unwrap().is_some());
        assert!(
            fs.file_block(
                &journal,
                (journal.size() / u64::from(geometry.block_size()) - 1) as u32
            )
            .unwrap()
            .is_some()
        );
        assert!(fs.read_at(&journal, 0, &mut bytes).unwrap() > 0);
        assert_eq!(&bytes[..4], &[0xc0, 0x3b, 0x39, 0x98]);
    }
    if geometry.has_orphan_file() {
        let orphan = fs.inode(geometry.orphan_file_inode()).unwrap();
        assert_eq!(orphan.mode() & 0xf000, 0x8000);
        assert!(fs.file_block(&orphan, 0).unwrap().is_some());
        assert!(fs.read_at(&orphan, 0, &mut bytes).unwrap() > 0);
    }
    println!(
        "EXT4_METADATA_CHECK_PASSED image={image} block_size={} groups={} root_size={} checksum_seed={:#x} orphan_file={}",
        geometry.block_size(),
        geometry.groups(),
        root.size(),
        geometry.checksum_seed(),
        geometry.has_orphan_file()
    );
}
