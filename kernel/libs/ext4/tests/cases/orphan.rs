// SPDX-License-Identifier: MPL-2.0

use super::*;

#[test]
fn truncates_and_extends_without_exposing_removed_bytes() {
    let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
    for size in [0, 33, 4096, 4133, 8032, 16384] {
        let disk = Disk::fixture();
        let mut fs = disk.write_session();
        let free = fs.geometry().free_blocks();
        fs.truncate(PAYLOAD_INODE, size, timestamp).unwrap();
        let kept = size.min(8032).div_ceil(BLOCK_SIZE as u64);
        assert_eq!(fs.geometry().free_blocks(), free + 2 - kept);
        let inode = fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap();
        assert_eq!(inode.size(), size);
        fs.truncate(PAYLOAD_INODE, 16384, timestamp).unwrap();
        let inode = fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap();
        let mut actual = vec![0x57; 16384];
        assert_eq!(
            fs.reader()
                .unwrap()
                .read_at(&inode, 0, &mut actual)
                .unwrap(),
            actual.len()
        );
        let original = (0..251u8).cycle().take(8032).collect::<Vec<_>>();
        assert_eq!(
            &actual[..size.min(8032) as usize],
            &original[..size.min(8032) as usize]
        );
        assert!(
            actual[size.min(8032) as usize..]
                .iter()
                .all(|byte| *byte == 0)
        );
        fs.close().unwrap();
        export(&disk, &format!("truncated-{size}-after.ext4"));
    }
}

#[test]
fn resumes_truncation_after_each_write_and_flush_failure() {
    let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
    for orphan_file in [false, true] {
        for size in [0, 4133] {
            fault_each_write(
                || {
                    if orphan_file {
                        Disk::fixture()
                    } else {
                        Disk::from_sparse(include_bytes!("../fixtures/classic.sparse"))
                    }
                },
                |fs| fs.truncate(PAYLOAD_INODE, size, timestamp),
                |fs, _, _, _| {
                    let inode = fs.inode(PAYLOAD_INODE).unwrap();
                    assert!([size, 8032].contains(&inode.size()));
                    let mut actual = vec![0; inode.size() as usize];
                    fs.read_at(&inode, 0, &mut actual).unwrap();
                    assert_eq!(
                        actual,
                        (0..251u8)
                            .cycle()
                            .take(inode.size() as usize)
                            .collect::<Vec<_>>()
                    );
                    for block in inode.size().div_ceil(BLOCK_SIZE as u64)..2 {
                        assert_eq!(fs.file_block(&inode, block as u32).unwrap(), None);
                    }
                },
            );
        }
    }
}

#[test]
fn leaves_linux_replayable_truncation_commits() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        let disk = interrupted_truncation(mode);
        export(&disk, &format!("truncate-commit-{mode:?}-before.ext4"));
        let fs = Ext4::recover(disk.clone(), limits()).unwrap();
        assert_eq!(fs.inode(PAYLOAD_INODE).unwrap().size(), 4133);
        export(&disk, &format!("truncate-commit-{mode:?}-after.ext4"));
    }
}

#[test]
fn resumes_interrupted_orphan_cleanup_after_each_write_and_flush_failure() {
    let base = interrupted_truncation(Mode::V3);
    let reference = base.snapshot();
    Ext4::recover(reference.clone(), limits()).unwrap();
    let operations = reference.0.borrow().operations;
    for cut in 0..operations {
        for retained in 0..3 {
            let disk = base.snapshot();
            disk.fault(cut);
            assert!(
                matches!(
                    Ext4::recover(disk.clone(), limits()),
                    Err(Error::Io(IoError::Failed))
                ),
                "cut {cut}, retained {retained}"
            );
            disk.crash(retained);
            let fs = Ext4::recover(disk, limits())
                .unwrap_or_else(|error| panic!("cut {cut}, retained {retained}: {error:?}"));
            let inode = fs.inode(PAYLOAD_INODE).unwrap();
            assert_eq!(inode.size(), 4133);
            assert_eq!(fs.file_block(&inode, 2).unwrap(), None);
            assert!(!fs.geometry().orphan_cleanup_required());
        }
    }
}

#[test]
fn retains_open_unlinked_files_until_explicit_reclamation() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    let free_blocks = fs.geometry().free_blocks();
    let free_inodes = fs.geometry().free_inodes();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let inode = fs.unlink(2, b"payload", changed).unwrap();
    assert_eq!(inode.links(), 0);
    let root = fs.reader().unwrap().inode(2).unwrap();
    assert!(matches!(
        fs.reader().unwrap().lookup(&root, b"payload"),
        Err(Error::NotFound)
    ));
    assert_eq!(fs.geometry().free_inodes(), free_inodes);
    let mut bytes = vec![0; inode.size() as usize];
    fs.reader().unwrap().read_at(&inode, 0, &mut bytes).unwrap();
    assert_eq!(bytes, (0..251u8).cycle().take(8032).collect::<Vec<_>>());
    write_payload(&mut fs, 16384, &[0x57]);
    fs.truncate(PAYLOAD_INODE, 33, changed).unwrap();
    fs.reap(PAYLOAD_INODE).unwrap();
    assert_eq!(fs.geometry().free_blocks(), free_blocks + 2);
    assert_eq!(fs.geometry().free_inodes(), free_inodes + 1);
    let inode = fs
        .create_file(2, b"replacement", created_metadata(7))
        .unwrap();
    assert_eq!(inode.number(), PAYLOAD_INODE);
    assert_eq!(inode.size(), 0);
    fs.close().unwrap();
    export(&disk, "unlinked-reused-after.ext4");
}

#[test]
fn recovers_unlink_and_reclamation_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for orphan_file in [false, true] {
        fault_each_write(
            || {
                if orphan_file {
                    Disk::fixture()
                } else {
                    Disk::from_sparse(include_bytes!("../fixtures/classic.sparse"))
                }
            },
            |fs| {
                fs.unlink(2, b"payload", changed)?;
                fs.reap(PAYLOAD_INODE)
            },
            |fs, _, _, _| {
                assert_payload_live_or_reclaimed(fs);
            },
        );
    }
}

#[test]
fn leaves_linux_replayable_unlink_commits() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        let log = Log::new(mode, 1, 42);
        let mut fs = log.start_session();
        let offset = fs.reader().unwrap().group(0).unwrap().inode_table() * BLOCK_SIZE as u64;
        log.disk.0.borrow_mut().fail_offset = Some(offset);
        assert!(matches!(
            fs.unlink(2, b"payload", Timestamp::new(1_600_000_001, 0).unwrap()),
            Err(Error::Io(IoError::Failed))
        ));
        drop(fs);
        log.disk.crash(0);
        export(&log.disk, &format!("unlink-{mode:?}-before.ext4"));
        let fs = log.recover().unwrap();
        let root = fs.inode(2).unwrap();
        assert!(matches!(fs.lookup(&root, b"payload"), Err(Error::NotFound)));
        export(&log.disk, &format!("unlink-{mode:?}-after.ext4"));
    }
}

#[test]
fn bounds_orphan_recovery_without_clearing_its_markers() {
    for orphan_file in [false, true] {
        let disk = Disk::orphan_fixture(orphan_file);
        let mut fs = disk.write_session();
        let changed = Timestamp::new(1_600_000_001, 0).unwrap();
        let file = fs.create_file(2, b"second", created_metadata(7)).unwrap();
        fs.unlink(2, b"payload", changed).unwrap();
        fs.unlink(2, b"second", changed).unwrap();
        drop(fs);
        disk.crash(0);
        let bounded = MountLimits {
            orphan_inodes: 1,
            ..limits()
        };
        assert!(matches!(
            Ext4::recover(disk.clone(), bounded),
            Err(Error::ResourceLimit)
        ));
        assert!(matches!(
            Ext4::open(disk.clone()),
            Err(Error::NeedsRecovery)
        ));
        let fs = Ext4::recover(disk, limits()).unwrap();
        for number in [PAYLOAD_INODE, file.number()] {
            assert!(matches!(fs.inode(number), Err(Error::Corrupt)));
        }
    }
}

#[test]
fn rejects_cyclic_or_duplicate_orphan_records_before_cleanup_writes() {
    for orphan_file in [false, true] {
        let disk = Disk::orphan_fixture(orphan_file);
        let fs = Ext4::open(disk.clone()).unwrap();
        let mut raw = disk.read(1024, 1024);
        if orphan_file {
            let file = fs.inode(fs.geometry().orphan_file_inode()).unwrap();
            let physical = fs.file_block(&file, 0).unwrap().unwrap();
            let mut block = disk.read(physical * BLOCK_SIZE as u64, BLOCK_SIZE);
            block[..4].copy_from_slice(&PAYLOAD_INODE.to_le_bytes());
            block[4..8].copy_from_slice(&PAYLOAD_INODE.to_le_bytes());
            let group = fs.group(0).unwrap();
            let offset =
                group.inode_table() * BLOCK_SIZE as u64 + u64::from(file.number() - 1) * 256;
            let generation = disk.read(offset + 100, 4);
            let crc = checksum::crc32c(fs.geometry().checksum_seed(), &file.number().to_le_bytes());
            let crc = checksum::crc32c(crc, &generation);
            let crc = checksum::crc32c(crc, &physical.to_le_bytes());
            let crc = checksum::crc32c(crc, &block[..BLOCK_SIZE - 8]);
            block[BLOCK_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
            disk.set(physical * BLOCK_SIZE as u64, &block);
            let features = u32::from_le_bytes(raw[0x64..0x68].try_into().unwrap());
            raw[0x64..0x68].copy_from_slice(&(features | 0x10000).to_le_bytes());
        } else {
            disk.mutate_inode(PAYLOAD_INODE, |inode| {
                inode[20..24].copy_from_slice(&PAYLOAD_INODE.to_le_bytes())
            });
            raw[0xe8..0xec].copy_from_slice(&PAYLOAD_INODE.to_le_bytes());
        }
        set_super_checksum(&mut raw);
        disk.set(1024, &raw);
        assert!(matches!(
            Ext4::recover(disk.clone(), limits()),
            Err(Error::Corrupt)
        ));
        assert_eq!(disk.0.borrow().operations, 0);
    }
}

#[test]
fn recovers_smaller_block_allocation_and_truncate_at_each_io_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for block_size in [1024, 2048] {
        for shrink in [false, true] {
            fault_each_write(
                || smaller_block_fixture(block_size),
                |fs| {
                    if shrink {
                        fs.truncate(PAYLOAD_INODE, 17, changed)
                    } else {
                        fs.write_at(PAYLOAD_INODE, 32768, &[0x57], changed)
                            .map(|_| ())
                    }
                },
                |fs, _, _, _| {
                    let inode = fs.inode(PAYLOAD_INODE).unwrap();
                    assert!([8032, if shrink { 17 } else { 32769 }].contains(&inode.size()));
                    let mut bytes = vec![0; inode.size() as usize];
                    fs.read_at(&inode, 0, &mut bytes).unwrap();
                    let prefix = bytes.len().min(8032);
                    assert_eq!(
                        &bytes[..prefix],
                        (0..251u8).cycle().take(prefix).collect::<Vec<_>>()
                    );
                    if inode.size() > 8032 {
                        assert!(bytes[8032..32768].iter().all(|byte| *byte == 0));
                        assert_eq!(bytes[32768], 0x57);
                    }
                },
            );
        }
    }
}

#[test]
fn creates_links_and_reclaims_special_inodes_with_checked_device_numbers() {
    let (disk, mut fs) = writable_fixture();
    let free = (fs.geometry().free_blocks(), fs.geometry().free_inodes());
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let character = DeviceNumber::new(4095, 1048575).unwrap();
    let block = DeviceNumber::new(259, 703710).unwrap();
    let mut created = Vec::new();
    for (index, (name, kind, device)) in [
        (b"fifo".as_slice(), SpecialFile::NamedPipe, None),
        (b"socket".as_slice(), SpecialFile::Socket, None),
        (
            b"char".as_slice(),
            SpecialFile::Character(character),
            Some(character),
        ),
        (b"block".as_slice(), SpecialFile::Block(block), Some(block)),
    ]
    .into_iter()
    .enumerate()
    {
        let inode = fs
            .create_special(2, name, kind, created_metadata(index as u32 + 1))
            .unwrap();
        assert_eq!(inode.device_number(), device);
        assert_eq!(inode.allocated_sectors(fs.geometry()), 0);
        created.push((name.to_vec(), inode.number()));
    }
    fs.link(2, b"fifo-alias", created[0].1, changed).unwrap();
    fs.close().unwrap();
    export(&disk, "special-inodes-after.ext4");
    let mut fs = disk.write_session();
    fs.unlink(2, b"fifo-alias", changed).unwrap();
    for (name, number) in created {
        fs.unlink(2, &name, changed).unwrap();
        fs.reap(number).unwrap();
    }
    assert_eq!(
        (fs.geometry().free_blocks(), fs.geometry().free_inodes()),
        free
    );
    fs.close().unwrap();
    export(&disk, "special-inodes-removed-after.ext4");
    assert!(matches!(
        DeviceNumber::new(4096, 0),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        DeviceNumber::new(0, 1048576),
        Err(Error::InvalidInput)
    ));
}

#[test]
fn links_live_temporary_files_without_leaking_orphan_records() {
    for orphan_file in [false, true] {
        let disk = Disk::orphan_fixture(orphan_file);
        let mut fs = disk.write_session();
        let free = (fs.geometry().free_blocks(), fs.geometry().free_inodes());
        let changed = Timestamp::new(1_600_000_001, 0).unwrap();
        let inode = fs.create_tmpfile(2, created_metadata(1)).unwrap();
        assert_eq!(inode.links(), 0);
        assert_eq!(
            fs.write_at(inode.number(), 8192, b"temporary", changed)
                .unwrap(),
            9
        );
        fs.link(2, b"linked-temp", inode.number(), changed).unwrap();
        assert_eq!(
            fs.reader().unwrap().inode(inode.number()).unwrap().links(),
            1
        );
        fs.close().unwrap();
        export(
            &disk,
            &format!("tmpfile-orphanfile{orphan_file}-after.ext4"),
        );
        let mut fs = disk.write_session();
        fs.unlink(2, b"linked-temp", changed).unwrap();
        fs.reap(inode.number()).unwrap();
        assert_free_counts(&fs, free);
        fs.close().unwrap();
    }
}

#[test]
fn recovers_anonymous_inode_allocation_at_each_write_and_flush_failure() {
    for orphan_file in [false, true] {
        fault_each_write(
            || {
                if orphan_file {
                    Disk::fixture()
                } else {
                    Disk::from_sparse(include_bytes!("../fixtures/classic.sparse"))
                }
            },
            |fs| fs.create_tmpfile(2, created_metadata(1)).map(|_| ()),
            |fs, _, _, _| {
                let number = if orphan_file { 14 } else { 12 };
                assert!(matches!(fs.inode(number), Err(Error::Corrupt)));
                let root = fs.inode(2).unwrap();
                assert_eq!(fs.lookup(&root, b"payload").unwrap().size(), 8032);
            },
        );
    }
}
