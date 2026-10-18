// SPDX-License-Identifier: MPL-2.0

use super::*;

#[test]
fn replays_all_checksum_formats_and_escaped_blocks() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        for escaped in [false, true] {
            let mut log = Log::new(mode, 1, 0x1234_5678);
            let mut data = vec![0x57; BLOCK_SIZE];
            if escaped {
                data[..4].copy_from_slice(&MAGIC.to_be_bytes());
            }
            log.data(log.target, &data);
            let label = format!("{mode:?}-escape{escaped}");
            export(&log.disk, &format!("{label}-before.ext4"));
            let fs = log.recover().unwrap();
            assert!(!fs.geometry().needs_recovery());
            assert!(!fs.geometry().orphan_cleanup_required());
            assert_eq!(fs.journal().unwrap().unwrap().start(), 0);
            assert_eq!(log.target_bytes(), data);
            export(&log.disk, &format!("{label}-after.ext4"));
            Ext4::open(log.disk.clone()).unwrap();
        }
    }
}

#[test]
fn ignores_incomplete_or_torn_commit() {
    for damaged_commit in [false, true] {
        let mut log = Log::new(Mode::V3, 1, 42);
        let original = log.overwrite_payload(0x57);
        if damaged_commit {
            log.corrupt(log.last_commit, 16);
        } else {
            log.disk.set(
                log.mapping[log.last_commit as usize] * BLOCK_SIZE as u64,
                &vec![0; BLOCK_SIZE],
            );
        }
        log.recover().unwrap();
        assert_eq!(log.target_bytes(), original);
    }
}

#[test]
fn rejects_committed_corruption_before_home_writes() {
    for (metadata, offset) in [(false, 20), (true, 20), (true, 0), (true, 8), (true, 4)] {
        let mut log = Log::new(Mode::V3, 1, 42);
        let original = log.overwrite_payload(0x57);
        log.corrupt(
            if metadata {
                log.last_descriptor
            } else {
                log.last_data
            },
            offset,
        );
        assert!(matches!(log.recover(), Err(Error::Corrupt)));
        assert_eq!(log.disk.0.borrow().operations, 0);
        assert_eq!(log.target_bytes(), original);
        assert!(matches!(
            Ext4::open(log.disk.clone()),
            Err(Error::NeedsRecovery)
        ));
    }
}

#[test]
fn ignores_torn_descriptor_without_valid_commit() {
    let mut log = Log::new(Mode::V3, 1, 42);
    let original = log.overwrite_payload(0x57);
    log.corrupt(log.last_descriptor, 20);
    log.corrupt(log.last_commit, 16);
    log.recover().unwrap();
    assert_eq!(log.target_bytes(), original);
}

#[test]
fn handles_ring_and_sequence_wrap_and_revocations() {
    let mut log = Log::new(Mode::V3, 1023, u32::MAX);
    let original = log.overwrite_payload(0x57);
    log.revoke(log.target);
    export(&log.disk, "revoked-before.ext4");
    log.recover().unwrap();
    assert_eq!(log.target_bytes(), original);

    let mut log = Log::new(Mode::V3, 1023, u32::MAX);
    log.data(log.target, &vec![0x57; BLOCK_SIZE]);
    log.revoke(log.target);
    let latest = vec![0x58; BLOCK_SIZE];
    log.data(log.target, &latest);
    export(&log.disk, "reused-before.ext4");
    log.recover().unwrap();
    assert_eq!(log.target_bytes(), latest);
}

#[test]
fn rejects_journal_self_overwrite_before_any_writes() {
    let mut log = Log::new(Mode::V3, 1, 42);
    log.data(log.mapping[2], &vec![0x57; BLOCK_SIZE]);
    assert!(matches!(log.recover(), Err(Error::Corrupt)));
    assert_eq!(log.disk.0.borrow().operations, 0);
}

#[test]
fn enforces_read_only_and_recovery_resource_limits() {
    let mut log = Log::new(Mode::V3, 1, 42);
    log.data(log.target, &vec![0x57; BLOCK_SIZE]);
    log.disk.0.borrow_mut().read_only = true;
    assert!(matches!(log.recover(), Err(Error::ReadOnly)));
    log.disk.0.borrow_mut().read_only = false;
    for limit in [
        MountLimits {
            journal_blocks: 0,
            ..limits()
        },
        MountLimits {
            transactions: 0,
            ..limits()
        },
        MountLimits {
            mapping_entries: 0,
            ..limits()
        },
    ] {
        assert!(matches!(
            Ext4::recover(log.disk.clone(), limit),
            Err(Error::ResourceLimit)
        ));
        assert_eq!(log.disk.0.borrow().operations, 0);
    }
}

#[test]
fn resumes_recovery_after_each_write_and_flush_failure() {
    let mut reference = Log::new(Mode::V3, 1, 42);
    let data = vec![0x57; BLOCK_SIZE];
    reference.data(reference.target, &data);
    reference.recover().unwrap();
    let operations = reference.disk.0.borrow().operations;
    for cut in 0..operations {
        for retained in 0..3 {
            let mut log = Log::new(Mode::V3, 1, 42);
            log.data(log.target, &data);
            log.disk.fault(cut);
            assert!(matches!(log.recover(), Err(Error::Io(IoError::Failed))));
            log.disk.crash(retained);
            log.recover().unwrap();
            assert_eq!(log.target_bytes(), data, "cut {cut}, retained {retained}");
            Ext4::open(log.disk.clone()).unwrap();
        }
    }
}

#[test]
fn commits_and_closes_inode_metadata_and_file_overwrites() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    let timestamp = Timestamp::new(1_600_000_001, 123_456_789).unwrap();
    fs.set_metadata(
        PAYLOAD_INODE,
        owner_change(0x1234_5678, 0x8765_4321, timestamp),
    )
    .unwrap();
    let inode = fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap();
    assert_eq!(inode.uid(), 0x1234_5678);
    assert_eq!(inode.gid(), 0x8765_4321);
    assert_eq!(
        fs.write_at(PAYLOAD_INODE, 12, &vec![0x57; 5000], timestamp)
            .unwrap(),
        5000
    );
    let mut bytes = vec![0; 5000];
    fs.reader()
        .unwrap()
        .read_at(&inode, 12, &mut bytes)
        .unwrap();
    assert_eq!(bytes, vec![0x57; 5000]);
    fs.close().unwrap();
    let fs = Ext4::open(disk.clone()).unwrap();
    assert!(fs.geometry().is_clean());
    assert!(!fs.geometry().needs_recovery());
    export(&disk, "writer-after.ext4");
}

#[test]
fn seals_failed_write_sessions_and_recovers_atomic_inode_changes() {
    let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
    fault_each_write(
        Disk::fixture,
        |fs| fs.set_metadata(PAYLOAD_INODE, owner_change(1000, 1001, timestamp)),
        |fs, _, cut, retained| {
            let inode = fs.inode(PAYLOAD_INODE).unwrap();
            assert!(
                matches!((inode.uid(), inode.gid()), (0, 0) | (1000, 1001)),
                "cut {cut}, retained {retained}"
            );
        },
    );
}

#[test]
fn reports_flush_failure_and_requires_explicit_recovery_after_drop() {
    let disk = Disk::fixture();
    let mut fs = disk.write_session();
    disk.fault(0);
    assert_eq!(fs.sync(), Err(Error::Io(IoError::Failed)));
    assert_eq!(fs.sync(), Err(Error::Aborted));
    drop(fs);
    disk.crash(0);
    assert!(matches!(
        Ext4::open(disk.clone()),
        Err(Error::NeedsRecovery)
    ));
    Ext4::recover(disk, limits()).unwrap();
}

#[test]
fn leaves_linux_replayable_commits_in_each_checksum_format() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        let log = Log::new(mode, 1, 42);
        let mut fs = log.start_session();
        log.disk.fault(7);
        let timestamp = Timestamp::new(1_600_000_001, 0).unwrap();
        assert!(matches!(
            fs.set_metadata(PAYLOAD_INODE, owner_change(300, 301, timestamp)),
            Err(Error::Io(IoError::Failed))
        ));
        drop(fs);
        log.disk.crash(0);
        export(&log.disk, &format!("writer-{mode:?}-before.ext4"));
        let fs = log.recover().unwrap();
        let inode = fs.inode(PAYLOAD_INODE).unwrap();
        assert_eq!((inode.uid(), inode.gid()), (300, 301));
    }
}

#[test]
fn leaves_linux_replayable_multi_block_creation_commits() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        let log = Log::new(mode, 1, 42);
        let mut fs = log.start_session();
        log.disk.0.borrow_mut().fail_offset = Some(0);
        assert!(matches!(
            fs.create_directory(2, b"work", created_metadata(1)),
            Err(Error::Io(IoError::Failed))
        ));
        drop(fs);
        log.disk.crash(0);
        export(&log.disk, &format!("mkdir-{mode:?}-before.ext4"));
        let fs = log.recover().unwrap();
        assert_work_directory(&fs);
    }
}

#[test]
fn replays_linux_descriptors_with_uninitialised_tag_uuid() {
    for mode in [Mode::None, Mode::V1, Mode::V2, Mode::V3] {
        let mut log = Log::new(mode, 1, 42);
        let data = vec![0x57; BLOCK_SIZE];
        log.data(log.target, &data);
        let physical = log.mapping[1];
        let mut raw = log.disk.read(physical * BLOCK_SIZE as u64, BLOCK_SIZE);
        let tag_bytes = if matches!(mode, Mode::V3) {
            16
        } else if matches!(mode, Mode::V2) {
            14
        } else {
            12
        };
        if matches!(mode, Mode::V3) {
            raw[16..20].copy_from_slice(&8u32.to_be_bytes());
        } else {
            raw[18..20].copy_from_slice(&8u16.to_be_bytes());
        }
        raw[12 + tag_bytes..28 + tag_bytes].fill(0);
        log.metadata_checksum(&mut raw);
        log.disk.set(physical * BLOCK_SIZE as u64, &raw);
        let transaction_crc = checksum::crc32_be(checksum::crc32_be(!0, &raw), &data);
        if matches!(mode, Mode::V1) {
            let offset = log.mapping[log.last_commit as usize] * BLOCK_SIZE as u64;
            let mut commit = log.disk.read(offset, BLOCK_SIZE);
            commit[16..20].copy_from_slice(&transaction_crc.to_be_bytes());
            log.disk.set(offset, &commit);
        }
        log.recover().unwrap();
        assert_eq!(log.target_bytes(), data);
    }
}
