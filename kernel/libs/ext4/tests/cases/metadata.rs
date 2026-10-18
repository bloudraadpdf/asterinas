// SPDX-License-Identifier: MPL-2.0

use super::*;

#[test]
fn enforces_immutable_and_append_only_inode_flags() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for flag in [0x10, 0x20] {
        let disk = Disk::fixture();
        disk.mutate_inode(PAYLOAD_INODE, |raw| {
            let flags = u32::from_le_bytes(raw[32..36].try_into().unwrap());
            raw[32..36].copy_from_slice(&(flags | flag).to_le_bytes());
        });
        let mut fs = disk.write_session();
        disk.0.borrow_mut().operations = 0;
        assert_eq!(
            fs.write_at(PAYLOAD_INODE, 0, &[0x57], changed),
            Err(Error::PermissionDenied)
        );
        assert_eq!(
            fs.truncate(PAYLOAD_INODE, 0, changed),
            Err(Error::PermissionDenied)
        );
        assert!(matches!(
            fs.unlink(2, b"payload", changed),
            Err(Error::PermissionDenied)
        ));
        assert_eq!(
            fs.set_metadata(PAYLOAD_INODE, owner_change(1000, 1001, changed)),
            Err(Error::PermissionDenied)
        );
        assert_eq!(disk.0.borrow().operations, 0);
        if flag == 0x20 {
            assert_eq!(
                fs.write_at(PAYLOAD_INODE, 8032, &[0x57], changed).unwrap(),
                1
            );
        }
        fs.close().unwrap();
    }
}

#[test]
fn updates_root_inode_metadata_without_admitting_reserved_inodes() {
    let (disk, mut fs) = writable_fixture();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    fs.set_metadata(2, owner_change(2000, 2001, changed))
        .unwrap();
    let root = fs.reader().unwrap().inode(2).unwrap();
    assert_eq!((root.uid(), root.gid()), (2000, 2001));
    for number in [1, 8, 12] {
        assert_eq!(
            fs.set_metadata(number, owner_change(2000, 2001, changed)),
            Err(Error::PermissionDenied)
        );
    }
    fs.close().unwrap();
    export(&disk, "root-metadata-after.ext4");
}

#[test]
fn preserves_timestamp_epochs_nanoseconds_and_file_type() {
    let (disk, mut fs) = writable_fixture();
    let accessed = Timestamp::new(-1, 123_456_789).unwrap();
    let modified = Timestamp::new(4_294_967_300, 987_654_321).unwrap();
    let changed = Timestamp::new(8_589_934_600, 999_999_999).unwrap();
    fs.set_metadata(
        PAYLOAD_INODE,
        MetadataUpdate {
            permissions: Some(0o6750),
            uid: Some(1000),
            gid: Some(1001),
            accessed: Some(accessed),
            modified: Some(modified),
            changed,
        },
    )
    .unwrap();
    let inode = fs.reader().unwrap().inode(PAYLOAD_INODE).unwrap();
    assert_eq!(inode.mode(), 0x8000 | 0o6750);
    assert_eq!(inode.access_time().unwrap(), accessed);
    assert_eq!(inode.modify_time().unwrap(), modified);
    assert_eq!(inode.change_time().unwrap(), changed);
    assert_eq!(
        inode.birth_time().unwrap().unwrap(),
        Timestamp::new(1_600_000_000, 0).unwrap()
    );
    assert_eq!(inode.allocated_sectors(fs.geometry()), 16);
    fs.close().unwrap();
    export(&disk, "timestamps-after.ext4");
}

#[test]
fn rejects_invalid_timestamp_nanoseconds_in_checked_inode_records() {
    let disk = Disk::fixture();
    disk.mutate_inode(PAYLOAD_INODE, |raw| {
        raw[140..144].copy_from_slice(&4_000_000_000u32.to_le_bytes())
    });
    let fs = Ext4::open(disk).unwrap();
    let inode = fs.inode(PAYLOAD_INODE).unwrap();
    assert_eq!(inode.access_time(), Err(Error::Corrupt));
}
