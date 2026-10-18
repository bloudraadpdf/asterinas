// SPDX-License-Identifier: MPL-2.0

use super::*;

#[test]
fn extends_files_with_zeroed_sparse_gaps() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    let free = fs.geometry().free_blocks();
    write_payload(&mut fs, 20_000, &vec![0x57; 5000]);
    let inode = fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap();
    assert_eq!(inode.size(), 25_000);
    assert_eq!(fs.geometry().free_blocks(), free - 3);
    let mut bytes = vec![0; 25_000];
    assert_eq!(
        fs.reader().unwrap().read_at(&inode, 0, &mut bytes).unwrap(),
        bytes.len()
    );
    assert_eq!(
        &bytes[..8032],
        (0..251u8).cycle().take(8032).collect::<Vec<_>>()
    );
    assert!(bytes[8032..20_000].iter().all(|byte| *byte == 0));
    assert!(bytes[20_000..].iter().all(|byte| *byte == 0x57));
    fs.close().unwrap();
    export(&disk, "sparse-extension-after.ext4");
}

#[test]
fn splits_extent_trees_and_updates_first_keys_in_both_directions() {
    for reverse in [false, true] {
        let disk = Disk::from_sparse(include_bytes!("../fixtures/indexed.sparse"));
        let mut fs = disk.write_session();
        let root = fs.reader().unwrap().inode(2).unwrap();
        let directory = fs.reader().unwrap().lookup(&root, b"indexed").unwrap();
        let number = fs
            .reader()
            .unwrap()
            .lookup(&directory, b"name0000")
            .unwrap()
            .number();
        let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
        let initial_free = fs.geometry().free_blocks();
        let count = 1500;
        for step in 0..count {
            let index = if reverse { count - 1 - step } else { step };
            let offset = index as u64 * 8192;
            assert_eq!(
                fs.write_at(number, offset, &[(index % 251) as u8], timestamp)
                    .unwrap(),
                1
            );
        }
        let reader = fs.reader().unwrap();
        let inode = reader.inode(number).unwrap();
        assert_eq!(inode.size(), (count - 1) as u64 * 8192 + 1);
        for index in 0..count {
            let mut bytes = [0; 2];
            let count = reader
                .read_at(&inode, index as u64 * 8192, &mut bytes)
                .unwrap();
            assert!(count > 0);
            assert_eq!(bytes[0], (index % 251) as u8);
            assert_eq!(bytes[1], 0);
            assert!(
                reader
                    .file_block(&inode, (index * 2 + 1) as u32)
                    .unwrap()
                    .is_none()
            );
        }
        fs.close().unwrap();
        export(&disk, &format!("extent-splits-reverse{reverse}-after.ext4"));
        let mut fs = disk.write_session();
        fs.truncate(number, 37 * 8192 + 17, timestamp).unwrap();
        fs.close().unwrap();
        export(&disk, &format!("extent-trim-reverse{reverse}-after.ext4"));
        let mut fs = disk.write_session();
        fs.truncate(number, 0, timestamp).unwrap();
        assert_eq!(fs.geometry().free_blocks(), initial_free);
        assert_eq!(fs.reader().unwrap().inode(number).unwrap().size(), 0);
        fs.close().unwrap();
        export(&disk, &format!("extent-empty-reverse{reverse}-after.ext4"));
    }
}

#[test]
fn recovers_file_allocation_after_each_write_and_flush_failure() {
    let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
    fault_each_write(
        Disk::fixture,
        |fs| {
            fs.write_at(PAYLOAD_INODE, 16384, &[0x57], timestamp)
                .map(|_| ())
        },
        |fs, _, cut, retained| {
            let inode = fs.inode(PAYLOAD_INODE).unwrap();
            assert!(
                matches!(inode.size(), 8032 | 16385),
                "cut {cut}, retained {retained}"
            );
            let mut bytes = vec![0; inode.size() as usize];
            fs.read_at(&inode, 0, &mut bytes).unwrap();
            assert_eq!(
                &bytes[..8032],
                (0..251u8).cycle().take(8032).collect::<Vec<_>>()
            );
            if inode.size() > 8032 {
                assert!(bytes[8032..16384].iter().all(|byte| *byte == 0));
                assert_eq!(bytes[16384], 0x57);
            }
        },
    );
}

#[test]
fn converts_unwritten_extents_without_exposing_old_disk_bytes() {
    let disk = Disk::fixture();
    disk.mutate_inode(PAYLOAD_INODE, |raw| {
        assert_eq!(u16::from_le_bytes(raw[42..44].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(raw[56..58].try_into().unwrap()), 2);
        raw[56..58].copy_from_slice(&32770u16.to_le_bytes());
    });
    let mut fs = disk.write_session();
    let free = fs.geometry().free_blocks();
    write_payload(&mut fs, 34, &[0x57]);
    write_payload(&mut fs, 4096 + 17, &[0x58]);
    assert_eq!(fs.geometry().free_blocks(), free);
    let inode = fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap();
    let mut actual = vec![0; 8032];
    fs.reader()
        .unwrap()
        .read_at(&inode, 0, &mut actual)
        .unwrap();
    let mut expected = vec![0; 8032];
    expected[34] = 0x57;
    expected[4096 + 17] = 0x58;
    assert_eq!(actual, expected);
    fs.close().unwrap();
    export(&disk, "unwritten-after.ext4");
}

#[test]
fn rejects_corrupt_bitmaps_and_free_reserved_metadata_before_writes() {
    for valid_checksum in [false, true] {
        let disk = Disk::fixture();
        let reader = Ext4::open(disk.clone()).unwrap();
        let group = reader.group(0).unwrap();
        let mut bitmap = disk.read(group.block_bitmap() * BLOCK_SIZE as u64, BLOCK_SIZE);
        bitmap[0] &= !1;
        disk.set(group.block_bitmap() * BLOCK_SIZE as u64, &bitmap);
        if valid_checksum {
            let geometry = reader.geometry();
            let mut descriptor = disk.read(BLOCK_SIZE as u64, geometry.descriptor_size() as usize);
            let crc = checksum::crc32c(geometry.checksum_seed(), &bitmap);
            descriptor[24..26].copy_from_slice(&(crc as u16).to_le_bytes());
            descriptor[56..58].copy_from_slice(&((crc >> 16) as u16).to_le_bytes());
            store_descriptor(&disk, reader.geometry().checksum_seed(), descriptor);
        }
        let mut fs = disk.write_session();
        disk.0.borrow_mut().operations = 0;
        let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
        assert_eq!(
            fs.write_at(PAYLOAD_INODE, 16384, &[0x57], timestamp),
            Err(Error::Corrupt)
        );
        assert_eq!(disk.0.borrow().operations, 0);
    }
}

#[test]
fn recounts_allocation_groups_before_writable_admission() {
    let disk = Disk::fixture();
    let reader = Ext4::open(disk.clone()).unwrap();
    let expected = (
        reader.geometry().free_blocks(),
        reader.geometry().free_inodes(),
    );
    let mut raw = disk.read(1024, 1024);
    raw[12..20].fill(0);
    raw[0x158..0x15c].fill(0);
    set_super_checksum(&mut raw);
    disk.set(1024, &raw);
    let mut fs = WritableExt4::open(disk, limits()).unwrap();
    assert_eq!(
        (fs.geometry().free_blocks(), fs.geometry().free_inodes()),
        expected
    );
    fs.create_file(2, b"recounted", created_metadata(1))
        .unwrap();
    fs.close().unwrap();
}

#[test]
fn rejects_last_group_counts_outside_the_volume() {
    for shrink_inode_count in [false, true] {
        let disk = Disk::fixture();
        let fs = Ext4::open(disk.clone()).unwrap();
        if shrink_inode_count {
            let mut raw = disk.read(1024, 1024);
            raw[..4].copy_from_slice(&8000u32.to_le_bytes());
            raw[0x10..0x14].copy_from_slice(&7987u32.to_le_bytes());
            set_super_checksum(&mut raw);
            disk.set(1024, &raw);
        } else {
            let mut descriptor = disk.read(BLOCK_SIZE as u64, 64);
            descriptor[12..14].copy_from_slice(&16385u16.to_le_bytes());
            store_descriptor(&disk, fs.geometry().checksum_seed(), descriptor);
        }
        let fs = Ext4::open(disk).unwrap();
        assert!(matches!(fs.group(0), Err(Error::Corrupt)));
    }
}

#[test]
fn initialises_lazy_inode_groups_and_reports_exhaustion() {
    for count in [3, 50] {
        let disk = Disk::from_sparse(include_bytes!("../fixtures/lazy-tables.sparse"));
        let mut fs = disk.write_session();
        assert_eq!(fs.geometry().groups(), 4);
        assert_eq!(fs.geometry().free_inodes(), 51);
        let directory = fs
            .create_directory(2, b"work", created_metadata(1))
            .unwrap();
        for index in 0..count {
            let name = format!("file{index:04}");
            let inode = fs
                .create_file(
                    directory.number(),
                    name.as_bytes(),
                    created_metadata(index + 2),
                )
                .unwrap();
            assert_eq!((inode.uid(), inode.gid()), (1000, 1001));
            assert_eq!(inode.size(), 0);
        }
        assert_eq!(fs.geometry().free_inodes(), 50 - count);
        if count == 50 {
            assert!(matches!(
                fs.create_file(directory.number(), b"full", created_metadata(100)),
                Err(Error::NoSpace)
            ));
            assert!(fs.reader().is_ok());
        }
        fs.close().unwrap();
        export(&disk, &format!("lazy-inodes-{count}-after.ext4"));
    }
}

#[test]
fn rejects_cross_group_metadata_aliases_before_writable_admission() {
    let disk = Disk::from_sparse(include_bytes!("../fixtures/lazy-tables.sparse"));
    let reader = Ext4::open(disk.clone()).unwrap();
    let bitmap = reader.group(0).unwrap().block_bitmap();
    let mut raw = disk.read(BLOCK_SIZE as u64 + 64, 64);
    raw[..4].copy_from_slice(&(bitmap as u32).to_le_bytes());
    raw[32..36].copy_from_slice(&((bitmap >> 32) as u32).to_le_bytes());
    raw[30..32].fill(0);
    let seed = checksum::crc32c(reader.geometry().checksum_seed(), &1u32.to_le_bytes());
    let crc = checksum::crc32c(seed, &raw);
    raw[30..32].copy_from_slice(&(crc as u16).to_le_bytes());
    disk.set(BLOCK_SIZE as u64 + 64, &raw);
    assert!(matches!(
        WritableExt4::open(disk.clone(), limits()),
        Err(Error::Corrupt)
    ));
    assert_eq!(disk.0.borrow().operations, 0);
}

#[test]
fn rejects_unallocated_file_blocks_before_overwrite() {
    let disk = Disk::fixture();
    let fs = Ext4::open(disk.clone()).unwrap();
    let inode = fs.inode(PAYLOAD_INODE).unwrap();
    let physical = fs.file_block(&inode, 0).unwrap().unwrap();
    let group = fs.group(0).unwrap();
    let mut bitmap = disk.read(group.block_bitmap() * BLOCK_SIZE as u64, BLOCK_SIZE);
    bitmap[physical as usize / 8] &= !(1 << (physical % 8));
    disk.set(group.block_bitmap() * BLOCK_SIZE as u64, &bitmap);
    let mut descriptor = disk.read(BLOCK_SIZE as u64, 64);
    let free = group.free_blocks() + 1;
    descriptor[12..14].copy_from_slice(&(free as u16).to_le_bytes());
    descriptor[44..46].copy_from_slice(&((free >> 16) as u16).to_le_bytes());
    let crc = checksum::crc32c(fs.geometry().checksum_seed(), &bitmap);
    descriptor[24..26].copy_from_slice(&(crc as u16).to_le_bytes());
    descriptor[56..58].copy_from_slice(&((crc >> 16) as u16).to_le_bytes());
    store_descriptor(&disk, fs.geometry().checksum_seed(), descriptor);
    let mut fs = disk.write_session();
    disk.0.borrow_mut().operations = 0;
    assert_eq!(
        fs.write_at(
            PAYLOAD_INODE,
            0,
            &[0x57],
            Timestamp::new(1_600_000_001, 0).unwrap()
        ),
        Err(Error::Corrupt)
    );
    assert_eq!(disk.0.borrow().operations, 0);
}

#[test]
fn mutates_smaller_blocks_and_clean_nonjournalled_volumes() {
    for (name, fixture) in [
        (
            "rw-1024",
            include_bytes!("../fixtures/rw-1024.sparse").as_slice(),
        ),
        (
            "rw-2048",
            include_bytes!("../fixtures/rw-2048.sparse").as_slice(),
        ),
        (
            "rw-4096-nojournal",
            include_bytes!("../fixtures/rw-4096-nojournal.sparse").as_slice(),
        ),
    ] {
        let disk = Disk::from_sparse(fixture);
        let mut bounded = limits();
        bounded.journal_blocks = 4096;
        let mut fs = WritableExt4::open(disk.clone(), bounded).unwrap();
        let free = (fs.geometry().free_blocks(), fs.geometry().free_inodes());
        let block_size = u64::from(fs.geometry().block_size());
        let changed = Timestamp::new(1_600_000_001, 123_456_789).unwrap();
        let directory = fs
            .create_directory(2, b"work", created_metadata(1))
            .unwrap();
        let file = fs
            .create_file(directory.number(), b"new", created_metadata(2))
            .unwrap();
        let data = (0..251u8)
            .cycle()
            .take((block_size * 6 + 17) as usize)
            .collect::<Vec<_>>();
        let mut written = 0;
        while written < data.len() {
            let count = fs
                .write_at(file.number(), written as u64, &data[written..], changed)
                .unwrap();
            assert!(count > 0);
            written += count;
        }
        let inode = fs.reader().unwrap().inode(file.number()).unwrap();
        let mut actual = vec![0; data.len()];
        fs.reader()
            .unwrap()
            .read_at(&inode, 0, &mut actual)
            .unwrap();
        assert_eq!(actual, data);
        fs.write_at(file.number(), 20 * block_size + 7, &[0x57], changed)
            .unwrap();
        fs.truncate(file.number(), 17, changed).unwrap();
        fs.set_metadata(
            file.number(),
            MetadataUpdate {
                permissions: Some(0o600),
                ..owner_change(1000, 1001, changed)
            },
        )
        .unwrap();
        fs.set_attribute(
            file.number(),
            1,
            b"tiny",
            &[0x57; 32],
            AttributeMode::Create,
            changed,
        )
        .unwrap();
        fs.set_attribute(
            file.number(),
            1,
            b"external",
            &[0x58; 300],
            AttributeMode::Create,
            changed,
        )
        .unwrap();
        fs.rename(
            directory.number(),
            b"new",
            directory.number(),
            b"moved",
            changed,
        )
        .unwrap();
        fs.link(directory.number(), b"alias", file.number(), changed)
            .unwrap();
        fs.unlink(directory.number(), b"moved", changed).unwrap();
        let link = fs
            .create_symlink(directory.number(), b"link", b"alias", created_metadata(3))
            .unwrap();
        assert_eq!(fs.reader().unwrap().read_link(&link).unwrap(), b"alias");
        assert_eq!(
            (fs.geometry().free_blocks(), fs.geometry().free_inodes()),
            (free.0 - 3, free.1 - 3)
        );
        let inode = fs.reader().unwrap().inode(file.number()).unwrap();
        assert_eq!(inode.allocated_sectors(fs.geometry()), 2 * block_size / 512);
        let mut bytes = [0; 17];
        fs.reader().unwrap().read_at(&inode, 0, &mut bytes).unwrap();
        assert_eq!(bytes, std::array::from_fn(|index| index as u8));
        fs.close().unwrap();
        export(&disk, &format!("{name}-after.ext4"));
    }
}

#[test]
fn retains_dirty_nonjournalled_markers_instead_of_claiming_recovery() {
    let disk = Disk::from_sparse(include_bytes!("../fixtures/rw-4096-nojournal.sparse"));
    let mut fs = disk.write_session();
    fs.create_file(2, b"new", created_metadata(1)).unwrap();
    drop(fs);
    disk.crash(0);
    assert!(matches!(
        Ext4::recover(disk.clone(), limits()),
        Err(Error::Unsupported)
    ));
    assert!(matches!(Ext4::open(disk), Err(Error::NeedsRecovery)));
}
