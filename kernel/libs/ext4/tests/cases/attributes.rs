// SPDX-License-Identifier: MPL-2.0

use super::*;

#[test]
fn reclaims_external_attributes_only_after_the_last_reference() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for shared in [false, true] {
        let disk = attribute_fixture(shared);
        let mut fs = disk.write_session();
        let free = fs.geometry().free_blocks();
        fs.unlink(2, b"payload", changed).unwrap();
        fs.reap(PAYLOAD_INODE).unwrap();
        assert_eq!(
            fs.geometry().free_blocks(),
            free + if shared { 2 } else { 3 }
        );
        fs.close().unwrap();
        export(&disk, &format!("attributes-shared{shared}-one-after.ext4"));
        let mut fs = disk.write_session();
        let inode = fs.unlink(2, b"shared", changed).unwrap();
        fs.reap(inode.number()).unwrap();
        assert_eq!(
            fs.geometry().free_blocks(),
            free + if shared { 5 } else { 6 }
        );
        fs.close().unwrap();
        export(&disk, &format!("attributes-shared{shared}-none-after.ext4"));
    }
}

#[test]
fn recovers_attribute_reclamation_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for shared in [false, true] {
        fault_each_write(
            || attribute_fixture(shared),
            |fs| {
                fs.unlink(2, b"payload", changed)?;
                fs.reap(PAYLOAD_INODE)
            },
            |fs, disk, _, _| {
                let root = fs.inode(2).unwrap();
                assert_eq!(fs.lookup(&root, b"shared").unwrap().size(), 8032);
                assert_payload_live_or_reclaimed(fs);
                let group = fs.group(0).unwrap();
                let raw = disk.read(group.inode_table() * BLOCK_SIZE as u64 + 13 * 256, 256);
                let physical = u64::from(u32::from_le_bytes(raw[104..108].try_into().unwrap()));
                let mut block = disk.read(physical * BLOCK_SIZE as u64, BLOCK_SIZE);
                let live = fs.lookup(&root, b"payload").is_ok();
                assert_eq!(
                    u32::from_le_bytes(block[4..8].try_into().unwrap()),
                    if shared && live { 2 } else { 1 }
                );
                let expected = u32::from_le_bytes(block[16..20].try_into().unwrap());
                block[16..20].fill(0);
                let seed = checksum::crc32c(fs.geometry().checksum_seed(), &physical.to_le_bytes());
                assert_eq!(checksum::crc32c(seed, &block), expected);
            },
        );
    }
}

#[test]
fn creates_replaces_and_removes_inline_and_external_attributes() {
    let (disk, mut fs) = writable_fixture();
    let free = fs.geometry().free_blocks();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let large = (0..251u8).cycle().take(1004).collect::<Vec<_>>();
    fs.set_attribute(2, 1, b"root", b"root-value", AttributeMode::Create, changed)
        .unwrap();
    fs.set_attribute(
        PAYLOAD_INODE,
        1,
        b"small",
        b"inline-value",
        AttributeMode::Create,
        changed,
    )
    .unwrap();
    assert_eq!(fs.geometry().free_blocks(), free);
    fs.set_attribute(
        PAYLOAD_INODE,
        1,
        b"large",
        &large,
        AttributeMode::Upsert,
        changed,
    )
    .unwrap();
    assert_eq!(fs.geometry().free_blocks(), free - 1);
    assert_attribute(&fs, PAYLOAD_INODE, b"large", &large);
    assert_eq!(
        attribute_value(fs.reader().unwrap(), PAYLOAD_INODE, b"small").unwrap(),
        b"inline-value"
    );
    assert_eq!(
        fs.set_attribute(
            PAYLOAD_INODE,
            1,
            b"small",
            b"duplicate",
            AttributeMode::Create,
            changed
        ),
        Err(Error::AlreadyExists)
    );
    assert_eq!(
        fs.set_attribute(
            PAYLOAD_INODE,
            1,
            b"absent",
            b"missing",
            AttributeMode::Replace,
            changed
        ),
        Err(Error::NoAttribute)
    );
    fs.set_attribute(
        PAYLOAD_INODE,
        1,
        b"small",
        b"replaced",
        AttributeMode::Replace,
        changed,
    )
    .unwrap();
    assert_eq!(
        attribute_value(fs.reader().unwrap(), PAYLOAD_INODE, b"small").unwrap(),
        b"replaced"
    );
    fs.close().unwrap();
    export(&disk, "updated-attributes-after.ext4");
    let mut fs = disk.write_session();
    fs.remove_attribute(PAYLOAD_INODE, 1, b"large", changed)
        .unwrap();
    assert_eq!(fs.geometry().free_blocks(), free);
    fs.remove_attribute(PAYLOAD_INODE, 1, b"small", changed)
        .unwrap();
    assert!(attribute_value(fs.reader().unwrap(), PAYLOAD_INODE, b"small").is_none());
    assert_eq!(
        fs.remove_attribute(PAYLOAD_INODE, 1, b"absent", changed),
        Err(Error::NoAttribute)
    );
    fs.close().unwrap();
    export(&disk, "updated-attributes-removed-after.ext4");
}

#[test]
fn copies_shared_attribute_blocks_without_changing_other_inodes() {
    let disk = attribute_fixture(true);
    let mut fs = disk.write_session();
    let free = fs.geometry().free_blocks();
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    let original = bytes_attribute();
    let replacement = vec![0x57; 1501];
    fs.set_attribute(
        PAYLOAD_INODE,
        1,
        b"large",
        &replacement,
        AttributeMode::Replace,
        changed,
    )
    .unwrap();
    assert_eq!(fs.geometry().free_blocks(), free - 1);
    assert_attribute(&fs, PAYLOAD_INODE, b"large", &replacement);
    assert_attribute(&fs, 14, b"large", &original);
    fs.close().unwrap();
    export(&disk, "attribute-cow-after.ext4");
}

#[test]
fn recovers_attribute_copy_on_write_after_each_write_and_flush_failure() {
    let changed = Timestamp::new(1_600_000_001, 0).unwrap();
    for remove in [false, true] {
        fault_each_write(
            || attribute_fixture(true),
            |fs| {
                if remove {
                    fs.remove_attribute(PAYLOAD_INODE, 1, b"large", changed)
                } else {
                    fs.set_attribute(
                        PAYLOAD_INODE,
                        1,
                        b"large",
                        &vec![0x57; 1501],
                        AttributeMode::Replace,
                        changed,
                    )
                }
            },
            |fs, _, _, _| {
                assert_eq!(
                    attribute_value(fs, 14, b"large").unwrap(),
                    bytes_attribute()
                );
                let value = attribute_value(fs, PAYLOAD_INODE, b"large");
                if remove {
                    assert!(value.is_none() || value == Some(bytes_attribute()));
                } else {
                    assert!(value == Some(bytes_attribute()) || value == Some(vec![0x57; 1501]));
                }
            },
        );
    }
}

#[test]
fn enables_attribute_feature_for_inline_only_updates() {
    let disk = Disk::fixture();
    let mut raw = disk.read(1024, 1024);
    let flags = u32::from_le_bytes(raw[0x5c..0x60].try_into().unwrap());
    raw[0x5c..0x60].copy_from_slice(&(flags & !8).to_le_bytes());
    set_super_checksum(&mut raw);
    disk.set(1024, &raw);
    let mut fs = disk.write_session();
    fs.set_attribute(
        2,
        1,
        b"root",
        b"inline",
        AttributeMode::Create,
        Timestamp::new(1_600_000_001, 0).unwrap(),
    )
    .unwrap();
    fs.close().unwrap();
    assert_ne!(
        u32::from_le_bytes(disk.read(1024 + 0x5c, 4).try_into().unwrap()) & 8,
        0
    );
    export(&disk, "inline-feature-after.ext4");
}

#[test]
fn reads_linux_inline_and_external_attributes() {
    for shared in [false, true] {
        let fs = Ext4::open(attribute_fixture(shared)).unwrap();
        for number in [PAYLOAD_INODE, 14] {
            assert_eq!(
                attribute_value(&fs, number, b"large").unwrap(),
                bytes_attribute()
            );
            assert_eq!(
                attribute_value(&fs, number, b"small").unwrap(),
                b"inline-value"
            );
        }
    }
}
